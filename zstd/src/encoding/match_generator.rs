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
use super::cost_model::HC_FORMAT_MINMATCH;
#[cfg(test)]
use super::cost_model::HC_MAX_LIT;
#[cfg(test)]
use super::cost_model::{
    HC_BITCOST_MULTIPLIER, HC_OPT_NUM, HC_PREDEF_THRESHOLD, HcOptState, HcOptimalCostProfile,
};
#[cfg(test)]
use super::cost_model::{HC_BLOCKSIZE_MAX, HC_MAX_LL, HC_MAX_ML, HC_MAX_OFF, HcOptPriceType};
use super::dfast::DfastMatchGenerator;
#[cfg(test)]
use super::hc::HC_MIN_MATCH_LEN;
#[cfg(test)]
use super::match_table::storage::HC3_HASH_LOG;
// FAST_HASH_FILL_STEP test-only re-export was tied to the legacy
// SuffixStore MatchGenerator's interleaved hash-fill stride. The
// upstream zstd-shape Fast kernel walks ip0 with kSearchStrength step-skip
// acceleration instead, so the constant has no consumer in the
// remaining live test set today.
#[cfg(test)]
use super::match_table::helpers::INCOMPRESSIBLE_SKIP_STEP;
use super::match_table::helpers::MIN_MATCH_LEN;
#[cfg(test)]
use super::match_table::helpers::common_prefix_len;
#[cfg(test)]
use super::opt::ldm::HcRawSeq;
#[cfg(test)]
use super::opt::types::{HcCandidateQuery, MatchCandidate};
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
// Bytes the dfast short hash reads (upstream zstd `mls = 5`). Seeding / lookahead
// guards use it so a position is only short-hashed once its full 5-byte key
// is in range.
pub(crate) const DFAST_SHORT_HASH_LOOKAHEAD: usize = 5;
pub(crate) const ROW_MIN_MATCH_LEN: usize = 5;
// Upstream zstd `clevels.h:31` at level 3 large-input bucket sets
// `hashLog = 17` (the long-hash table) and `chainLog = 16` (the
// short-hash table — upstream zstd names this `chainTable` even though for
// dfast it's used as a plain single-slot hash). Each table holds one
// `U32` per slot; the upstream zstd overwrites on collision and recovers
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
// 4-slot bucket per hash position; that paid 4× the upstream zstd memory
// without measurable ratio gain once the retry was in place.
//
// `dfast_hash_bits_for_window` still clamps the runtime long-hash
// value to `[MIN_WINDOW_LOG, DFAST_HASH_BITS]`, so this const is the
// upper bound rather than a fixed default.
pub(crate) const DFAST_HASH_BITS: usize = 17;
/// Difference between `long_hash_bits` and `short_hash_bits` —
/// upstream zstd `hashLog - chainLog` is 1 at every dfast level (`clevels.h`
/// level 2: 16-15=1; level 3: 17-16=1). The short hash is one bit
/// smaller than the long hash so the per-bucket footprint matches
/// upstream zstd sizing exactly.
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
// HC3_MAX_OFFSET moved to encoding::bt alongside the hash3 candidate
// probe macro that consumes it; the macro references it via the
// fully-qualified `$crate::encoding::bt::HC3_MAX_OFFSET` path so this
// module no longer needs a local import.
pub(crate) const HC_SEARCH_DEPTH: usize = 16;
// HC_MIN_MATCH_LEN moved to encoding::hc; re-imported here so
// existing references compile unchanged.
pub(crate) const HC_OPT_MIN_MATCH_LEN: usize = HC_FORMAT_MINMATCH;
pub(crate) const HC_TARGET_LEN: usize = 48;

// MAX_HC_SEARCH_DEPTH moved to encoding::hc alongside chain_candidates.
// Per-level tuning config (the config structs + `LEVEL_TABLE` + the
// level→params resolution chain) lives in `levels::config`; the driver imports
// that resolution API here.
use super::levels::config::*;
// The HashChain / BT match generator + its optimal-parse machinery lives in
// `hc::generator`; the driver stores it in the `HashChain` storage variant.
use super::hc::generator::HcMatchGenerator;

// `Strategy` and `StrategyTag` live in `crate::encoding::strategy`.
// The driver carries a `StrategyTag` field set at `reset()` and
// dispatches each block into a monomorphised `compress_block::<S>`
// per concrete strategy.

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
    /// Upstream zstd `ZSTD_fast` family. Constructed by
    /// [`MatchGeneratorDriver::new`] as the initial variant and
    /// re-selected by [`Matcher::reset`] for any [`CompressionLevel`]
    /// that `resolve_level_params` maps to [`StrategyTag::Fast`]
    /// (`Uncompressed`, `Fastest`, `Level(1)`, and any non-positive
    /// `Level(n)` not equal to `0`).
    Simple(FastKernelMatcher),
    /// Upstream zstd `ZSTD_dfast` family — two-table hash chain. Selected for
    /// any level that resolves to [`StrategyTag::Dfast`] in
    /// `resolve_level_params` (`Default`, `Level(0)`, `Level(2)`,
    /// `Level(3)`).
    Dfast(DfastMatchGenerator),
    /// Upstream zstd `ZSTD_greedy` family with row hashing. Selected for any
    /// level that resolves to [`StrategyTag::Greedy`] (currently
    /// `Level(4)` only).
    Row(RowMatchGenerator),
    /// Upstream zstd `ZSTD_lazy2` and the BT-based optimal modes
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
    pub(crate) parse: super::strategy::ParseMode,
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
    // match-finder hash/chain tables are sized from the DICTIONARY (upstream zstd CDict
    // economics: a loaded dictionary supplies the long matches, so the live tables
    // can shrink to the dict's size tier) while the eviction window stays
    // source-sized. Mirrors upstream zstd `ZSTD_getCParamRowSize`, which picks the cParams
    // table column from `dictSize` for a dictionary-bearing compress.
    dictionary_size_hint: Option<usize>,
    // Normalized `ceil_log2` bucket of the frame's source-size hint, captured at
    // `reset` (where `source_size_hint` is consumed) via [`source_size_ceil_log`].
    // `None` means the frame was unhinted. Drives `prime_with_dictionary`'s upstream zstd
    // `ZSTD_shouldAttachDict` mode for the Simple/Fast backend: `None` (unknown)
    // or `<= FAST_ATTACH_DICT_CUTOFF_LOG` → attach (separate dict table, 2-cursor
    // `compress_block_fast_dict`); larger → copy (dictionary primed into the live
    // table, 4-cursor `compress_block_fast`). The primed-snapshot key is the
    // resolved shape ([`reset_shape`](Self::reset_shape)), not this bucket.
    reset_size_log: Option<u8>,
    // Whether the loaded dictionary fits the Fast attach path's tagged position
    // field (`<= MAX_FAST_ATTACH_DICT_REGION`). Captured at `reset` from the
    // dict-size hint (which equals the actual dict length on load) so the Fast
    // attach decision, the attach-epoch reset bit, and the primed-snapshot
    // `fast_attach` bit all gate on it consistently. `true` when there is no
    // dictionary (the attach path is then unused). A dict too large to tag falls
    // back to copy mode instead of overflowing the packed position.
    reset_dict_attach_ok: bool,
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
    /// mirroring upstream zstd `ZSTD_compressBegin_usingCDict` copying the
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
/// one upstream zstd tier (its `<= 16/128/256 KiB` thresholds). Keying on the raw hint
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
            // Deferred table: `new` runs before any source size or resolved
            // LevelParams exist, so allocating at the level-default hash_log
            // here would be thrown away by the first frame's reset (which
            // clamps the window to the input and reallocs at the resolved
            // size). The deferral lets that first reset allocate exactly once.
            storage: MatcherStorage::Simple(FastKernelMatcher::with_params_deferred(
                window_log_init,
                FAST_LEVEL_1_HASH_LOG,
                FAST_LEVEL_1_MLS,
                2, // upstream zstd default step_size (targetLength=0 → step=2)
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
            reset_dict_attach_ok: true,
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
    pub(crate) fn active_backend(&self) -> super::strategy::BackendTag {
        self.storage.backend()
    }

    /// Whether the borrowed (no-copy, in-place over-window) scan is
    /// implemented for the current backend + search configuration. The
    /// HashChain backend serves both the lazy CHAIN parser
    /// (`SearchMethod::HashChain`) and the BT/optimal parsers
    /// (`SearchMethod::BinaryTree`); only the lazy chain has a borrowed scan
    /// so far, so BT/optimal stay on the owned path.
    pub(crate) fn borrowed_supported(&self) -> bool {
        use super::strategy::{BackendTag, SearchMethod, StrategyTag};
        match self.active_backend() {
            BackendTag::Simple | BackendTag::Dfast | BackendTag::Row => true,
            // The HashChain backend covers two searches: the lazy CHAIN parser
            // (borrowed-capable) and the BINARY-TREE search (btlazy2 L13-15 +
            // optimal BtOpt/BtUltra/BtUltra2 L16-22). btlazy2's BT-tree borrowed
            // scan is byte-identical to owned (reads via live_history()), so it
            // takes the in-place path. The OPTIMAL parsers stay owned: their
            // cost-based DP is sensitive to candidate quality, and the borrowed
            // continuous-index scan yields slightly different (ratio-worse)
            // candidates than the owned evict+rehash scan — borrowed optimal
            // both diverged from owned and fell outside the ffi ratio bound.
            // Search-aware (not just strategy_tag) so optimal BT can never be
            // staged on the borrowed path even via an internal caller.
            BackendTag::HashChain => match self.search {
                SearchMethod::HashChain => true,
                SearchMethod::BinaryTree => matches!(self.strategy_tag, StrategyTag::Btlazy2),
                _ => false,
            },
        }
    }

    /// Whether a DICTIONARY frame can take the borrowed (no input copy) path.
    /// Only the Simple (Fast) backend with the dictionary ATTACHED (not the
    /// copy/merge regime) has a borrowed dict scan — `start_matching_borrowed_dict`
    /// reads live matches from the borrowed input in place and dict matches
    /// from the committed dict prefix via the 2-segment counter. Every other
    /// backend, and copy-mode (large-input) dict frames, stay on the owned
    /// path. Checked AFTER priming, so `is_attached()` reflects the resolved
    /// attach-vs-copy decision.
    pub(crate) fn borrowed_dict_supported(&self) -> bool {
        matches!(
            &self.storage,
            MatcherStorage::Simple(m) if m.dict_is_attached()
        )
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
            super::strategy::BackendTag::Row => unsafe {
                self.row_matcher_mut().set_borrowed_window(buffer)
            },
            super::strategy::BackendTag::HashChain => unsafe {
                self.hc_matcher_mut().set_borrowed_window(buffer)
            },
        }
    }

    /// Clear the borrowed one-shot window, returning the active backend
    /// to the owned `history` path.
    pub(crate) fn clear_borrowed_window(&mut self) {
        match self.active_backend() {
            super::strategy::BackendTag::Simple => self.simple_mut().clear_borrowed_window(),
            super::strategy::BackendTag::Dfast => self.dfast_matcher_mut().clear_borrowed_window(),
            super::strategy::BackendTag::Row => self.row_matcher_mut().clear_borrowed_window(),
            super::strategy::BackendTag::HashChain => self.hc_matcher_mut().clear_borrowed_window(),
            #[allow(unreachable_patterns)]
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
            self.borrowed_supported(),
            "borrowed block staging is not supported for the active backend/search config",
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
            super::strategy::BackendTag::Row => self
                .row_matcher_mut()
                .stage_borrowed_block(block_start, block_end),
            super::strategy::BackendTag::HashChain => self
                .hc_matcher_mut()
                .table
                .stage_borrowed_block(block_start, block_end),
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
    pub(crate) fn row_matcher(&self) -> &RowMatchGenerator {
        match &self.storage {
            MatcherStorage::Row(m) => m,
            _ => panic!("row backend must be initialized by reset() before use"),
        }
    }

    pub(crate) fn row_matcher_mut(&mut self) -> &mut RowMatchGenerator {
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
                    // flat `Vec<u8>` (upstream zstd's flat-buffer layout)
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

    /// ATTACH (`true`) vs COPY (`false`) decision for the dms-bearing HashChain
    /// backend (lazy hash-chain AND binary-tree/optimal levels), mirroring
    /// upstream `ZSTD_shouldAttachDict` and its per-strategy `attachDictSizeCutoffs`:
    /// a small / unknown source ATTACHES the dict as a separate dms (hash-chain
    /// dms for lazy, DUBT dms for BT); a large known source COPIES it into the
    /// live chain / tree. The cutoff is the lazy/lazy2 value for HC, the
    /// btlazy2/btopt value for Bt{Opt}, and the smaller btultra/btultra2 value for
    /// the deepest parses. Both `skip_matching_for_dictionary_priming` (which
    /// stages the dict) and `prime_with_dictionary` (which builds-or-drops the
    /// dms) read this so the two stay in lock-step.
    fn hc_dict_attach_mode(&self) -> bool {
        // Only the HashChain backend (lazy hash-chain + BT/optimal) routes here;
        // a non-HashChain storage has no dms decision, so default to attach.
        let MatcherStorage::HashChain(hc) = &self.storage else {
            return true;
        };
        let cutoff = if hc.table.uses_bt {
            match hc.strategy_tag {
                super::strategy::StrategyTag::BtUltra | super::strategy::StrategyTag::BtUltra2 => {
                    BT_ULTRA_ATTACH_DICT_CUTOFF_LOG
                }
                _ => BT_OPT_ATTACH_DICT_CUTOFF_LOG,
            }
        } else {
            HC_ATTACH_DICT_CUTOFF_LOG
        };
        self.reset_size_log.is_none_or(|log| log <= cutoff)
    }

    fn skip_matching_for_dictionary_priming(&mut self) {
        match self.active_backend() {
            super::strategy::BackendTag::Simple => {
                // Upstream zstd `ZSTD_shouldAttachDict` mode selection for the Fast
                // strategy (cutoff 8 KB): small / unknown-size inputs ATTACH
                // (index dict positions into a SEPARATE immutable table; the
                // dual-probe 2-cursor `compress_block_fast_dict` then prefers
                // recent-input matches and falls back to the dict — the path
                // that wins small/unknown). Large known-size inputs COPY (prime
                // dict into the live table; the 4-cursor `compress_block_fast`
                // matches against it as window history — the path that already
                // matches/beats the upstream zstd on large corpora). The dispatch in
                // `start_matching` keys off `dict_table.is_some()`, which only
                // the attach path populates. See [`FAST_ATTACH_DICT_CUTOFF_LOG`].
                let attach = self.reset_dict_attach_ok
                    && self
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
                // Upstream zstd `ZSTD_dictMatchState` mode selection for dfast (cutoff
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
                // Upstream zstd `ZSTD_RowFindBestMatch` `dictMatchState`: small /
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
                // Lazy-HC AND BT/optimal both follow upstream zstd `ZSTD_shouldAttachDict`
                // per-strategy: ATTACH (a separate dms — hash-chain dms for lazy,
                // DUBT dms for BT) for small / unknown inputs, COPY (merge the dict
                // into the live chain/tree) for large known inputs. ATTACH keeps
                // the dict in history but out of the live structure via
                // `skip_matching_dict_bt` (the cursor advance is shared by both
                // arms); COPY routes through the normal `skip_matching` (its
                // `uses_bt` branch fills the live tree, the lazy branch the live
                // chain). The dms is built-or-dropped to match in
                // `prime_with_dictionary`.
                if self.hc_dict_attach_mode() {
                    self.hc_matcher_mut().table.skip_matching_dict_bt();
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

    /// Dict-relevance gate for the raw-fast-path. Reached only when a dictionary
    /// is active (the caller short-circuits on `dict_active`), so this answers
    /// "could the dict compress this otherwise-incompressible-looking block?".
    /// The Simple (Fast) backend samples its dict table precisely
    /// ([`FastKernelMatcher::block_samples_match_dict`]); the other backends
    /// (Dfast / Row / HashChain / BT) have their own dict structures and no cheap
    /// probe here, so they answer CONSERVATIVELY `true`: without a probe they
    /// cannot tell whether the dict compresses an incompressible-LOOKING block,
    /// and answering `false` would let the raw-fast-path emit such a block raw
    /// and miss an embedded dict segment. `dictionary_segment_in_incompressible_input_is_matched`
    /// pins this for Dfast/Row/BT — the 512-byte dict run inside high-entropy
    /// filler is matched only because these backends stay on the scan. So they
    /// keep the blanket scan the old `!dict_active` gate gave them; only the
    /// Simple/Fast backend trades it for the precise probe.
    fn block_samples_match_dict(&self, block: &[u8]) -> bool {
        match &self.storage {
            MatcherStorage::Simple(m) => m.block_samples_match_dict(block),
            _ => true,
        }
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
        // A dictionary too large for the tagged attach position field falls back
        // to copy mode. Captured here (from the load-set size hint = actual dict
        // length) so the prime decision and the snapshot-key / epoch bits agree.
        self.reset_dict_attach_ok =
            dict_hint.is_none_or(|size| size <= MAX_FAST_ATTACH_DICT_REGION);
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
        // Dictionary-driven table sizing — parity with upstream zstd `ZSTD_createCDict`
        // (`ZSTD_getCParams_internal(level, UNKNOWN, dictSize, ZSTD_cpm_createCDict)`
        // → `ZSTD_adjustCParams_internal`). A loaded dictionary supplies the
        // long-distance matches, so upstream zstd sizes the prepared match-finder tables
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
        // collapses the HC/BT tables toward the 513-byte upstream zstd tier via
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
        // upstream zstd `ZSTD_resolveRowMatchFinderMode` (zstd_compress.c:238-245):
        // the row matchfinder is used for greedy/lazy/lazy2 ONLY when
        // `windowLog > 14`; at or below that upstream runs the hash-chain
        // matcher (`ZSTD_HcFindBestMatch`). We previously hardcoded the Row
        // backend for these strategies regardless of window, sending every
        // small-window frame (hinted floor = windowLog 14, e.g. the small-4k/10k
        // fixtures) through Row where upstream uses HC. Match it: fall back to
        // the hash-chain matcher (lazy/greedy parse via `lazy_depth`) when the
        // resolved window is <= 14. The HC config is synthesised from the
        // level's RowConfig (HC and Row share the same cParams; only the
        // matchfinder differs) — `hash_log` / `chain_log` are
        // clamped to the (<= 14) window inside the HashChain reset arm, so the
        // nominal width here only sets the clamp ceiling.
        if params.search == super::strategy::SearchMethod::RowHash && params.window_log <= 14 {
            let row = params
                .row
                .expect("a RowHash level row must carry a RowConfig");
            params.search = super::strategy::SearchMethod::HashChain;
            // For a dict-bearing frame, downsize the synthesised HC logs to the
            // dictionary's content tier via `cdict_table_logs` (the same
            // correction the native HC dict-prime path applies above), so a dict
            // much smaller than the window doesn't prime a needlessly sparse
            // table. Row-finder levels are never BinaryTree, so `uses_bt = false`.
            //
            // Feed `cdict_table_logs` the UN-hinted base Row width, not the
            // resolved `row.hash_bits`: the latter is already source-capped on a
            // hinted reset (the `row_cap = table_log + 1` clamp), so passing it
            // here would double-cap exactly as the native HC dict path warns
            // above — a small hinted source with a large dictionary would
            // collapse the prepared table below what the dict needs.
            // `cdict_table_logs` only ever downsizes, so deriving the ceiling
            // from the un-hinted base (plus active public overrides) keeps the
            // dict-tier geometry intact. No source hint => `row.hash_bits` is
            // already the level's full width, so reuse it directly.
            let row_cdict_hash_bits = match dict_hint.filter(|&size| size > 0) {
                Some(_) => {
                    let mut base_params = Self::level_params(level, None);
                    if let Some(ov) = self.param_overrides
                        && !ov.is_empty()
                    {
                        apply_param_overrides(&mut base_params, &ov);
                    }
                    base_params
                        .row
                        .map_or(row.hash_bits, |base_row| base_row.hash_bits)
                }
                None => row.hash_bits,
            };
            // Row-backed levels carry only `hash_bits`; the HC chain table they
            // fall back to follows the upstream zstd cParams relationship `chainLog =
            // hashLog - 1` for every Row level (L6 c18 h19 .. L12 c22 h23, see
            // the ROW_L* tables). Synthesise the chain width as `hash_bits - 1`
            // so the dict path doesn't leave the chain table one bit too wide
            // (cdict_table_logs only downsizes, so passing the full hash width
            // for both would keep a 2x-too-large chain table on dict frames).
            // Raw `- 1` is underflow-safe: `hash_bits` is either a predefined
            // ROW_L* width (>= 19) or a public `hash_log` override, and the
            // override is range-validated to `ZSTD_HASHLOG_MIN = 6` at the
            // parameter API, so the value is always >= 6 here.
            //
            // A public `chain_log` override (#27) is dropped by the RowHash
            // override arm (Row has no chain table), but once this frame falls
            // back to HC the chain table is live and must honour it — mirror
            // the native HC dict path, which feeds the override-applied
            // `base_hc.chain_log` into `cdict_table_logs`. Use the explicit
            // override (also API-validated to ZSTD_CHAINLOG_MIN = 6) when set,
            // else the upstream zstd `hashLog - 1` relationship.
            let explicit_chain_log = self
                .param_overrides
                .filter(|ov| !ov.is_empty())
                .and_then(|ov| ov.chain_log)
                .map(|chain_log| chain_log as usize);
            let row_cdict_chain_bits = explicit_chain_log.unwrap_or(row_cdict_hash_bits - 1);
            let (mut hash_log, mut chain_log) = match dict_hint.filter(|&size| size > 0) {
                Some(dict_size) => cdict_table_logs(
                    params.window_log,
                    row_cdict_hash_bits,
                    row_cdict_chain_bits,
                    false,
                    dict_size,
                ),
                None => (
                    row.hash_bits,
                    explicit_chain_log.unwrap_or(row.hash_bits - 1),
                ),
            };
            // No-dict path: the HashChain reset arm only clamps the logs to the
            // window when `hinted`, but a public `window_log` override can lower
            // this level to <= 14 with no source hint — clamp the level's full
            // Row `hash_bits` to the window here too (upstream zstd `ZSTD_adjustCParams`:
            // hashLog <= windowLog + 1, chainLog <= windowLog) so a 16 KiB window
            // doesn't allocate Row-sized HC tables.
            if dict_hint.filter(|&size| size > 0).is_none() {
                let wlog = params.window_log as usize;
                hash_log = hash_log.min(wlog + 1);
                chain_log = chain_log.min(wlog);
            }
            params.hc = Some(HcConfig {
                hash_log,
                chain_log,
                search_depth: row.search_depth,
                target_len: row.target_len,
                search_mls: 4,
            });
            params.row = None;
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
                    // short-circuits on `if !self.tables.is_empty()`, so
                    // handing it an empty `Vec` skips the fill loop.
                    // Mirrors the pre-drain pattern in the HashChain
                    // arm below (and serves the same peak-memory
                    // purpose: release the table-allocation footprint
                    // before constructing the replacement variant).
                    m.tables = Vec::new();
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
                    // get upstream zstd row-0 (hash_log=13, mls=7); Fastest /
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
                    && self.reset_dict_attach_ok
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
                // Cap `hash_log <= window_log + 1` (upstream zstd
                // `ZSTD_adjustCParams_internal`): once `window_log` is resized
                // down for a small source, a level-default `1 << hash_log`
                // table is mostly wasted address space whose per-frame memset
                // dominates the compress cost on tiny frames (a 4 KB frame at
                // window_log 12 still zero-fills the 64 KiB hash_log-14 table).
                // Gated to no-dict frames: the dict-attach path shares one
                // hash_log between the main and dict tables (so one hash keys
                // both), and shrinking only the main table would break that
                // invariant and the small-frame dict ratio.
                let hash_log = if dict_hint.is_some_and(|s| s > 0) {
                    fast.hash_log
                } else {
                    fast.hash_log.min(params.window_log as u32 + 1)
                };
                m.reset(
                    params.window_log,
                    hash_log,
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
                // Upstream zstd `cParams.hashLog`/`chainLog`, capped by the
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
                    // (upstream zstd `ZSTD_adjustCParams` caps hashLog by windowLog) —
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
                // input doesn't allocate the full level's tables (the upstream zstd
                // `ZSTD_adjustCParams_internal` clamp: `hashLog <= windowLog + 1`,
                // and `cycleLog <= windowLog` — `cycleLog == chainLog` for the HC
                // finder, `chainLog - 1` for the BT pair table, so `chainLog <=
                // windowLog` (+1 for BT)). Ratio-neutral: a hinted window of
                // `2^wlog` bytes holds at most `2^wlog` positions, so the slots
                // beyond that are never populated — capping only sheds unused
                // allocation. Was the source of L10-lazy peak-alloc ~2.15x the
                // upstream zstd on a 1 MiB input. Only applied when hinted; an
                // unknown-size stream keeps the full level tables.
                // Skip for dict-bearing frames: their `hc_cfg.{hash,chain}_log`
                // were already sized to the dictionary content tier via
                // `cdict_table_logs` (the dict supplies the long-distance
                // matches, so upstream `ZSTD_createCDict` sizes the prepared
                // tables to the dict, not the source window). Re-applying the
                // source-window cap here would collapse those dict-tier logs
                // back to the small hinted source — the same double-cap the
                // synthesis sites avoid by using the un-hinted base width.
                if hinted && !matches!(dict_hint, Some(size) if size > 0) {
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
                // does not overshoot via Vec capacity doubling (upstream zstd sizes its
                // window buffer exactly). Dominates peak once the match-finder
                // tables are dictionary-tier-small. Unhinted streams skip this
                // and keep doubling growth.
                if let Some(src) = hint {
                    // `src` is a u64 hint and may be the u64::MAX "unknown
                    // size" sentinel, which truncates under `as usize` on
                    // 32-bit targets and overflows when the dict hint is
                    // added. Saturate the source size, then saturate the
                    // dict-hint addition; `reserve_history` applies the
                    // tighter window ceiling to the result.
                    let src_hint = usize::try_from(src).unwrap_or(usize::MAX);
                    let expected = src_hint.saturating_add(dict_hint.unwrap_or(0));
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
        {
            // Resolve the derived LDM params first (immutable borrow of the
            // overrides), then reuse the existing producer's allocation below.
            let derived_ldm = self
                .param_overrides
                .as_ref()
                .and_then(|ov| ov.ldm)
                .map(|ldm_ov| {
                    let strategy_ord = ldm_strategy_ordinal(params.strategy_tag, params.lazy_depth);
                    // Seed the caller-pinned knobs, then run the upstream zstd
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
                    seed.derive(strategy_ord)
                });
            if let MatcherStorage::HashChain(hc) = &mut self.storage {
                // Reuse the existing producer's hash-table allocation when the
                // derived params are unchanged: only `clear()` (re-zero the
                // table + re-seed the rolling hash, no allocation) is needed for
                // the new frame. A params change (or the first frame) forces a
                // fresh `LdmProducer::new`. On the reused-encoder compress-dict
                // path this avoids re-allocating the LDM hash table (large at
                // btultra2) every frame — upstream zstd reuses its `ldmState_t`
                // the same way. `clear()` is mandatory here for correctness
                // regardless of what `BtMatcher::reset` did to the old table.
                let producer = derived_ldm.map(|p| match hc.take_ldm_producer() {
                    Some(mut existing) if existing.params() == p => {
                        existing.clear();
                        existing
                    }
                    _ => super::ldm::LdmProducer::new(p),
                });
                hc.set_ldm_producer(producer);
            }
        }
        // Record the resolved matcher shape for the primed-snapshot key. Captured
        // here (post-resolution, after the test-only param override) so the key
        // reflects exactly the geometry the restored `storage` must match. The
        // Fast attach-vs-copy mode is part of the shape ONLY for the Simple
        // backend (it decides the distinct dict-table shape that backend builds).
        // Dfast/Row/HashChain have their OWN attach/copy regimes, but this bit
        // models only the Fast table split; those backends are keyed by the
        // resolved matcher geometry instead, so folding the Fast bit into their
        // key would over-key identical resolved shapes. When it applies it
        // matches the decision `prime_with_dictionary` makes from the same
        // `reset_size_log`.
        let fast_attach = matches!(next_backend, super::strategy::BackendTag::Simple)
            && self.reset_dict_attach_ok
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

    fn dictionary_is_resident(&self) -> bool {
        match &self.storage {
            MatcherStorage::HashChain(hc) => hc.table.dict_resident,
            MatcherStorage::Simple(s) => s.dict_resident(),
            MatcherStorage::Dfast(d) => d.dict_resident(),
            _ => false,
        }
    }

    fn reapply_resident_dictionary(&mut self, offset_hist: [u32; 3]) {
        // Same offset-history head as `prime_with_dictionary`, without the dict
        // commit / re-index (resident dict bytes + cached dms already in place).
        match self.active_backend() {
            super::strategy::BackendTag::Simple => {
                self.simple_mut().prime_offset_history(offset_hist)
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
        // Restore the retained-dictionary budget the per-frame `reset` cleared.
        // The matcher's `reset` re-inflated `max_window_size` by the resident
        // dict region (so the dict + next input both stay in the eviction band),
        // exactly as `prime_with_dictionary` does — but the resident path skips
        // that prime, so without this the driver-level budget stays 0 and
        // `retire_dictionary_budget` never shrinks the inflated window as input
        // evicts the dict. For HashChain (whose `window_low` is measured against
        // `max_window_size`), a stuck-inflated window would let a post-eviction
        // match exceed the frame header's base window and emit an over-window
        // offset. The inflation equals `max_window_size - base`, and
        // `reported_window_size` is the base `1 << window_log` set by `reset`.
        let base = self.reported_window_size;
        let inflated = match self.active_backend() {
            super::strategy::BackendTag::Simple => self.simple_mut().max_window_size,
            super::strategy::BackendTag::Dfast => self.dfast_matcher_mut().max_window_size,
            super::strategy::BackendTag::Row => self.row_matcher_mut().max_window_size,
            super::strategy::BackendTag::HashChain => self.hc_matcher_mut().table.max_window_size,
        };
        self.dictionary_retained_budget = inflated.saturating_sub(base);
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
                // Clear the chain/hash tables (deferred from the dict-active
                // `reset`): prime rebuilds them from the dict, so they must start
                // empty. The reuse hot path skips prime and `clone_from`s a clean
                // snapshot instead, so only the first-prime / key-mismatch frames
                // pay the fill -- not every reused-CDict frame.
                matcher.table.clear_chain_hash_tables();
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
        use super::match_table::storage::MAX_PRIMED_WINDOW_SIZE;

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
            // Stage the dict chunk WITHOUT `get_next_space`'s
            // `resize(slice_size, 0)` zero-fill: that memsets a full
            // block-sized buffer (up to ~128 KiB) every frame only to have it
            // `clear()`-ed and overwritten by the dict bytes on the very next
            // lines — pure waste (measured ~10% of the small dict encode).
            // Reuse a pooled buffer's capacity if one is free (the prime/skip
            // cycle recycles them back), else allocate exactly the chunk.
            // Mirrors upstream zstd, which references the CDict content rather
            // than zero-filling a fresh window per frame.
            let mut space = self.vec_pool.pop().unwrap_or_default();
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
            // Recompute the lazy-HC attach decision made per-chunk in
            // `skip_matching_for_dictionary_priming` (stable across the prime —
            // `reset_size_log` does not change here).
            //
            // The HC attach/copy mode is deliberately NOT folded into `PrimedKey`
            // (unlike Fast `fast_attach`). Fast attach builds a separate dict
            // table whose dimensions differ from the copy-mode live table, so a
            // cross-mode restore would install mismatched table geometry and the
            // encoder could search past the frame window (undecodable). The two
            // HC modes share identical window geometry: `max_window_size` and the
            // dictionary limit are both set ABOVE this branch (the same value in
            // either mode), and the live chain table dimensions come from the
            // resolved `params` the key already pins. The modes differ only in
            // WHERE the committed dict lives — a single-link `dms` (attach) vs
            // merged into the live chain (copy) — both producing valid matches at
            // in-window offsets. Upstream zstd makes the same observation: attach
            // (`ZSTD_resetCCtx_byAttachingCDict`) and copy
            // (`ZSTD_resetCCtx_byCopyingCDict`) both keep the caller's
            // `windowLog`; the choice is a memory/speed trade-off, not a wire
            // contract. So restoring an attach snapshot where this frame would
            // have copied (or vice versa) yields a decodable frame that may only
            // differ in which matches are found (ratio) — algorithmic freedom, not
            // a defect. Keying on the mode would instead force a re-prime across
            // the cutoff, re-adding the per-frame cost this snapshot path removes.
            //
            // In practice the public reuse path (`compress_independent_frame`)
            // only ever captures AND restores the COPY-mode snapshot — capture is
            // gated on the above-cutoff source size, so a restored frame always
            // matches the captured mode. `hc_dict_snapshot_reuse_roundtrips` pins
            // that same-mode reuse decodes; the driver-level cross-mode restore is
            // accepted (not refused) per
            // `primed_snapshot_fast_attach_does_not_over_key_non_simple_backends`.
            let attach = self.hc_dict_attach_mode();
            let table = &mut self.hc_matcher_mut().table;
            table.set_dictionary_limit_from_primed_bytes(committed_dict_budget);
            // Build the dictMatchState over the committed dict (front of history)
            // so `find_best_match` dual-probes it with its own compare budget —
            // but ONLY in ATTACH mode. BT/optimal attach → DUBT dms; lazy-HC
            // attach → single-link hash-chain dms. COPY mode (large known source,
            // both BT and lazy-HC) already merged the dict into the live tree /
            // chain in `skip_matching_for_dictionary_priming`, so it carries no
            // separate dms — drop any stale one.
            if !attach {
                table.dms.invalidate();
            } else if table.uses_bt {
                table.prime_dms_bt(committed_dict_budget);
            } else {
                table.prime_dms_hc(committed_dict_budget);
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
            // dict-table buffers, so this is the upstream zstd CDict table-copy
            // regime's cost (pure copies) instead of a full per-frame
            // allocation + copy + drop cycle.
            (MatcherStorage::Simple(live), MatcherStorage::Simple(snap)) => {
                live.clone_from(snap);
            }
            // Same-variant HC lazy/greedy restore (non-BT): the snapshot keeps
            // the full primed hash/chain tables (capture's non-BT full clone),
            // so `clone_from` reuses the live history/hash/chain/dms buffers in
            // place — upstream zstd reuses the CDict tables rather than reallocating
            // them. This is the per-frame allocate+copy+drop that dominated
            // small `compress-dict` HC frames (5-7x vs C). BT (`uses_bt`)
            // snapshots drop their live tables, so they stay on the realloc
            // path below.
            (MatcherStorage::HashChain(live), MatcherStorage::HashChain(snap))
                if !snap.table.uses_bt =>
            {
                live.table.clone_from(&snap.table);
                live.hc.clone_from(&snap.hc);
                live.strategy_tag = snap.strategy_tag;
                // backend is `HcBackend::Hc` (zero-sized) for non-BT levels;
                // the live one is already correct for this resolved key.
            }
            (live, snapshot_storage) => {
                let mut storage = snapshot_storage.clone();
                // This arm handles the binary-tree backend. In ATTACH mode the
                // snapshot was stored WITHOUT its live hash / chain / hash3
                // tables (they hold no dictionary entries — the dict lives in
                // `dms` + history; see `capture_primed_dictionary`), so
                // `ensure_tables` re-allocates them zeroed to the snapshot's
                // geometry, exactly reproducing the post-prime state (all
                // `HC_EMPTY`). In COPY mode the snapshot retained its FULL live
                // tree (the dict was merged into it, no `dms`), so the tables are
                // already present at the right length and `ensure_tables` — which
                // only allocates on a length mismatch — leaves them untouched.
                // Either way this is a full storage replace, so no stale
                // live-table entry from a prior frame can survive.
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
        // CDict-equivalent retained state. A binary-tree level in ATTACH mode
        // decouples the dictionary into `dms` (the upstream zstd `dictMatchState`); its
        // live hash / chain / hash3 tables carry NO dict entries
        // (`skip_matching_dict_bt` keeps the dict out of the live tree), so they
        // are pure zeros. Storing them in the snapshot wastes the full table
        // footprint (a second window-tier table set resident for the whole
        // compress). Instead, move the live tables OUT of the working storage,
        // clone only the dict-state (history + `dms` + window/offset/dict-limit),
        // then move the live tables back — the snapshot keeps just what upstream zstd's
        // CDict keeps, and `restore_primed_dictionary` re-allocates the zeroed
        // live tables. Every other case keeps the dict reachable through the live
        // structure, so the snapshot must retain the full tables (full clone):
        // lazy-HC attach (it DOES prime a hash-chain `dms`, but the live chain is
        // still the search structure, so the tables must travel) and COPY mode for
        // BOTH BT and lazy-HC (`dms` invalidated, dict merged into the live tree /
        // chain). `uses_bt && dms.is_primed()` is therefore the exact "decoupled"
        // signal — true only for the BT attach prime; lazy-HC attach primes `dms`
        // too but is intentionally NOT decoupled.
        let bt_decoupled = matches!(
            &self.storage,
            MatcherStorage::HashChain(hc) if hc.table.uses_bt && hc.table.dms.is_primed()
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
                let table = &mut self.hc_matcher_mut().table;
                table.dms.invalidate();
                // Deactivate the dictionary state so the next `reset` does not
                // take the dict-active defer-the-table-clear branch. That branch
                // rewinds the tables to the origin and hands the clear off to a
                // following `prime_with_dictionary` / `restore_primed_dictionary`.
                // After a dictionary is removed (or replaced), the very next
                // frame may carry no dictionary, in which case neither hand-off
                // runs and the deferred clear would never execute — leaving stale
                // dict-region entries at the rewound base. Clearing the flag
                // routes that reset down the no-dictionary path instead; a
                // replacement dictionary re-arms the flag when it re-primes.
                table.dictionary_active = false;
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
                // Plain `+` (the `saturating_sub` floors at 0): `pre` + one
                // block are byte counts bounded by the window, no overflow.
                evicted_bytes += (pre + space_len).saturating_sub(m.window_size);
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
                // Plain `+` (the `saturating_sub` floors at 0): `pre` + one
                // block are byte counts bounded by the window, no overflow.
                evicted_bytes += (pre + space_len).saturating_sub(m.window_size);
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
                // Plain `+` (the `saturating_sub` floors at 0): byte counts
                // bounded by the window, no overflow.
                evicted_bytes += (pre + space_len).saturating_sub(m.table.window_size);
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
                super::strategy::BackendTag::Simple => {
                    let m = self.simple_mut();
                    if m.dict_is_attached() {
                        // Dict-attach borrowed scan: live matches read the
                        // borrowed input in place, dict matches read the
                        // committed dict prefix via the 2-segment counter.
                        m.start_matching_borrowed_dict(
                            block_start,
                            block_end,
                            &mut handle_sequence,
                        );
                    } else {
                        m.start_matching_borrowed(block_start, block_end, &mut handle_sequence);
                    }
                }
                super::strategy::BackendTag::Dfast => self
                    .dfast_matcher_mut()
                    .start_matching_borrowed(block_start, block_end, &mut handle_sequence),
                super::strategy::BackendTag::Row => {
                    // Same greedy/lazy parse split as the owned RowHash arm.
                    let greedy = self.parse == super::strategy::ParseMode::Greedy;
                    self.row_matcher_mut().start_matching_borrowed(
                        block_start,
                        block_end,
                        greedy,
                        &mut handle_sequence,
                    );
                }
                super::strategy::BackendTag::HashChain => match self.search {
                    super::strategy::SearchMethod::HashChain => self
                        .hc_matcher_mut()
                        .start_matching_lazy_borrowed(block_start, block_end, &mut handle_sequence),
                    super::strategy::SearchMethod::BinaryTree => {
                        // Run the SAME BT dispatch as the owned BinaryTree arm
                        // below — every BT body reads its range via
                        // current_block_range() and bytes via live_history()
                        // (borrowed-aware), so the staged block is scanned in
                        // place. The table was already staged by
                        // `set_borrowed_block` (the HashChain arm at the top of
                        // this file calls `table.stage_borrowed_block` with the
                        // same range, and `borrowed_pending` is set only there),
                        // so no re-stage is needed here.
                        // Only btlazy2 reaches the borrowed BinaryTree scan:
                        // `borrowed_supported()` keeps the optimal parsers
                        // (BtOpt/BtUltra/BtUltra2) on the owned path, and
                        // `set_borrowed_block` asserts that predicate before any
                        // range is staged, so an optimal strategy_tag can never
                        // arrive here.
                        match self.strategy_tag {
                            StrategyTag::Btlazy2 => self
                                .hc_matcher_mut()
                                .start_matching_btlazy2(&mut handle_sequence),
                            other => unreachable!(
                                "borrowed BinaryTree scan is only supported for Btlazy2, got {other:?}"
                            ),
                        }
                    }
                    other => {
                        unreachable!("HashChain backend with unexpected search {other:?}")
                    }
                },
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
                // Greedy parse (depth 0) = upstream zstd-greedy entry (default
                // `ip + 1` start, greedy repcode commit); lazy / lazy2 use
                // the `pick_lazy_match` lookahead entry (reads `lazy_depth`).
                // Both bare entries dispatch on `row_log` internally into the
                // const-`ROW_LOG` hot loop (upstream zstd per-rowLog variant table).
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
                super::strategy::BackendTag::Row => self.row_matcher_mut().skip_matching_borrowed(
                    block_start,
                    block_end,
                    incompressible_hint,
                ),
                super::strategy::BackendTag::HashChain => self
                    .hc_matcher_mut()
                    .skip_matching_borrowed(block_start, block_end, incompressible_hint),
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
// parse discipline (no lazy lookahead, upstream zstd `ZSTD_dfast`), NOT the Row/Greedy
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
    // `Best` sits on level 13 (the first dominant point of the deep band).
    check(CompressionLevel::Best, StrategyTag::Btlazy2);
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
    // Upstream zstd `clevels.h` (srcSize > 256 KiB tier): level 18 = `ZSTD_btultra`,
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
fn level22_uses_target_length_and_large_input_tables() {
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
fn level22_source_size_hint_uses_btultra2_tiers() {
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
fn level22_non_power_of_two_small_source_uses_tier3_params() {
    // srcSize 15 027 (<= 16 KB) selects the table[3] btultra2 row; the
    // source-size clamp gives windowLog 14 (ceil log2 15027). Pure-Rust
    // assertion against the constant tier-3 geometry (no FFI).
    let source_size = 15_027u64;
    let params = resolve_level_params(CompressionLevel::Level(22), Some(source_size));

    let hc = params.hc.unwrap();
    assert_eq!(params.window_log, 14);
    assert_eq!(hc.chain_log, 15);
    assert_eq!(hc.hash_log, 15);
    assert_eq!(hc.search_depth, 1 << 10);
    assert_eq!(HC_OPT_MIN_MATCH_LEN, 3);
    assert_eq!(hc.target_len, 999);
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
    // profile — the upstream zstd `opt2` pricing — so a single binding
    // captures the invariant the test is asserting.
    let profile = HcOptimalCostProfile::const_for_strategy::<super::strategy::BtUltra2>();
    assert!(
        !profile.favor_small_offsets,
        "btultra2 should match upstream zstd opt2 offset pricing"
    );
    assert!(
        profile.accurate,
        "btultra2 should use upstream zstd opt2 accurate pricing"
    );
}

#[test]
fn btultra_profile_keeps_search_depth_budget() {
    let p = HcOptimalCostProfile::const_for_strategy::<super::strategy::BtUltra>();
    assert_eq!(
        p.max_chain_depth, 64,
        "btultra chain-depth budget must match clevels.h level 18 searchLog 6 (1 << 6 = 64)"
    );
}

#[test]
fn btopt_profile_keeps_search_depth_budget() {
    let p = HcOptimalCostProfile::const_for_strategy::<super::strategy::BtOpt>();
    assert_eq!(
        p.max_chain_depth, 32,
        "btopt should not cap chain depth below upstream zstd btopt search budget"
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
        "chain fast-skip must not exceed upstream zstd-style matchEndIdx - 8 bound"
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
        "plain BT path should advance skip window by 1 via upstream zstd matchEndIdx baseline"
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
// + upstream zstd-parity ratio gate.

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
        "btultra2 should retain dictionary candidates on upstream zstd-parity path"
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
    // Upstream zstd-parity split sizes: long-hash = DFAST_HASH_BITS,
    // short-hash = DFAST_HASH_BITS - DFAST_SHORT_HASH_BITS_DELTA.
    let full_long = driver.dfast_matcher().long_len();
    let full_short = driver.dfast_matcher().short_len();
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
    let hinted_long = driver.dfast_matcher().long_len();
    let hinted_short = driver.dfast_matcher().short_len();

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
        driver.dfast_matcher().long_len() >= 1 << MIN_WINDOW_LOG,
        "huge hint must size the dfast table from the real window, not wrap to zero"
    );
}

#[test]
fn driver_huge_source_hint_with_dict_does_not_overflow_hc_reserve() {
    // Regression: the HC/BT history-mirror pre-size adds the dictionary
    // hint to the source-size hint before `reserve_history` clamps to the
    // window ceiling. A `u64::MAX` pledged source size (the "unknown size"
    // sentinel) plus any positive dictionary hint overflows `usize` in
    // `(src as usize) + dict_hint` — debug panic / release wrap on 64-bit,
    // and `src as usize` truncation on 32-bit targets. Level 16 (BtOpt)
    // routes through the HashChain/BT storage arm that owns this reserve.
    // Must size the mirror to the real window, never panic, wrap, or
    // truncate.
    let mut driver = MatchGeneratorDriver::new(32, 2);
    driver.set_source_size_hint(u64::MAX);
    driver.set_dictionary_size_hint(64 * 1024);
    driver.reset(CompressionLevel::Level(16));

    // The saturated `usize::MAX` reserve target must be clamped to the HC
    // history ceiling, not reserved literally (which would OOM/panic). Level 16
    // has window_log 22, so the ceiling is `window + window/4 + one block`
    // (the `reserve_history` formula). Assert the reserve actually reached it —
    // a no-panic-only check would also pass on an under-reserved mirror.
    let window = 1usize << 22;
    let expected_history_ceiling = window + (window >> 2) + crate::common::MAX_BLOCK_SIZE as usize;
    assert!(
        driver.hc_matcher().table.history.capacity() >= expected_history_ceiling,
        "huge source + dict hint must reserve the clamped HC history ceiling, got {}",
        driver.hc_matcher().table.history.capacity()
    );

    let mut space = driver.get_next_space();
    space[..12].copy_from_slice(b"abcabcabcabc");
    space.truncate(12);
    driver.commit_space(space);
    driver.skip_matching_with_hint(None);
}

#[test]
fn driver_chain_log_override_survives_row_to_hc_fallback() {
    // Regression: when a RowHash level is forced onto the HashChain backend
    // (resolved window <= 14, upstream `ZSTD_resolveRowMatchFinderMode`), the
    // synthesised HC chain table must honour an explicit `chain_log` override.
    // The RowHash override arm drops `chain_log` (Row has no chain table), so
    // the synthesis previously replaced the caller's `chain_log` with the upstream zstd
    // `hashLog - 1`, silently ignoring it on small-window frames.
    let chain_log_override = 10u32;
    let ov = super::parameters::ParamOverrides {
        chain_log: Some(chain_log_override),
        ..Default::default()
    };
    let mut driver = MatchGeneratorDriver::new(32, 2);
    // Small source hint pins the window to the hinted floor (16 KiB =
    // windowLog 14), so the Level 6 Row finder falls back to HashChain.
    driver.set_source_size_hint(1 << 12);
    driver.set_param_overrides(Some(ov));
    driver.reset(CompressionLevel::Level(6));
    let mut space = driver.get_next_space();
    space[..12].copy_from_slice(b"abcabcabcabc");
    space.truncate(12);
    driver.commit_space(space);
    driver.skip_matching_with_hint(None);
    // The override (10) is below the window cap (14), so the resolved HC chain
    // table must reflect it — NOT the upstream zstd `hashLog - 1` (18, clamped to the
    // window 14). Pre-fix this resolved to 14.
    assert_eq!(
        driver.hc_matcher().table.chain_log,
        chain_log_override as usize,
        "explicit chain_log override must survive the Row->HC fallback, got {}",
        driver.hc_matcher().table.chain_log
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
    // Level 5 uses the upstream row_log (clamp(searchLog=3, 4, 6) = 4) and the
    // upstream L5 hashLog (`ZSTD_getCParams(5,..).hashLog` = 19), so the row
    // count is 1 << (ROW_L5.hash_bits - ROW_L5.row_log).
    assert_eq!(full_rows, 1 << (ROW_L5.hash_bits - ROW_L5.row_log));

    // A hint that keeps the resolved window > 14 STILL uses the Row finder
    // (upstream `ZSTD_resolveRowMatchFinderMode`: row mode on for windowLog > 14)
    // and shrinks the row hash table to the source-derived width. 64 KiB →
    // raw source log 16, so `row_hash_bits_for_window(1 << 16)` < the level's
    // full hash_bits (19) and the row count drops.
    driver.set_source_size_hint(1 << 16);
    driver.reset(CompressionLevel::Level(5));
    let mut space = driver.get_next_space();
    space[..12].copy_from_slice(b"xyzxyzxyzxyz");
    space.truncate(12);
    driver.commit_space(space);
    driver.skip_matching_with_hint(None);
    assert_eq!(
        driver.active_backend(),
        super::strategy::BackendTag::Row,
        "windowLog > 14 keeps the upstream row matchfinder"
    );
    let hinted_rows = driver.row_matcher().row_heads.len();
    assert!(
        hinted_rows < full_rows,
        "a window>14 source hint should reduce the row hash table footprint"
    );

    // A tiny hint floors the resolved window at MIN_HINTED_WINDOW_LOG = 14;
    // upstream uses the HASH-CHAIN matcher (not Row) at windowLog <= 14, so the
    // driver must route greedy/lazy/lazy2 to the HashChain backend there.
    driver.set_source_size_hint(1024);
    driver.reset(CompressionLevel::Level(5));
    assert_eq!(driver.window_size(), 1 << MIN_HINTED_WINDOW_LOG);
    assert_eq!(
        driver.active_backend(),
        super::strategy::BackendTag::HashChain,
        "windowLog <= 14 must fall back to the upstream zstd hash-chain matchfinder",
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
/// Upstream zstd parity (`ZSTD_row_fillHashCache` only pre-fills the next-scan
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
         (upstream zstd parity — interior NOT inserted, no pre-block backfill possible)",
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

    // Upstream zstd-parity split: long-hash at DFAST_HASH_BITS, short-hash one
    // bit smaller (DFAST_SHORT_HASH_BITS_DELTA = 1, matching upstream zstd
    // `chainLog = hashLog - 1` for dfast levels).
    let long_len = driver.dfast_matcher().long_len();
    let short_len = driver.dfast_matcher().short_len();
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

    // Initialize at Best routed onto HashChain via the test-only override
    // (production `Best` sits on level 13, whose native backend differs) —
    // allocates large HC tables (4M hash, 2M chain) so the swap below
    // exercises the HC drain path this test pins.
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
// disabled: tests legacy SuffixStore behavior incompatible with upstream zstd-shape kernel's HASH_READ_SIZE geometry
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
// disabled: tests legacy SuffixStore behavior incompatible with upstream zstd-shape kernel's HASH_READ_SIZE geometry
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
    // Level(4) is Dfast with the greedy double-fast loop (upstream zstd parity:
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
fn primed_snapshot_restored_across_level22_tier_hints() {
    // Level 22 collapses several ceil-log buckets onto one upstream zstd source-size
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
        "precondition: both hints must land in the same Level 22 upstream zstd tier \
         (a={window_a}, b={window_b})"
    );

    let restored = driver.restore_primed_dictionary(level);
    assert!(
        restored,
        "Level 22 snapshot captured at a 20 KiB hint must be restored into a \
         100 KiB hint that resolves to the same upstream zstd tier (different ceil-log \
         buckets, identical matcher shape)"
    );
}

#[test]
fn fast_dict_attaches_within_cutoff_bounds() {
    // Within the attach bounds, every Fast dict frame attaches (the copy-mode
    // owned path memmoved the whole input into history each frame; attach scans
    // the input in place via the borrowed dual-base kernel). All hints here sit
    // far below `FAST_ATTACH_DICT_CUTOFF_LOG` (2 GiB source) and the dict is far
    // below `MAX_FAST_ATTACH_DICT_REGION` (16 MiB), so a hint that used to cross
    // the old 8 KiB cutoff (8193 B) and a small one (8192 B) BOTH resolve to
    // attach, and the Simple backend reports a borrowed (in-place) dict scan for
    // both. This guards `FAST_ATTACH_DICT_CUTOFF_LOG` staying high enough that no
    // in-bounds Fast hint falls back to the input-copy path; the OUT-of-bounds
    // fallbacks are covered by `fast_attach_cutoff_keeps_virtual_positions_within_u32`
    // (source) and `oversized_dict_hint_routes_fast_to_copy_mode` (dict size).
    let level = CompressionLevel::Level(1);
    for hint in [8192u64, 8193, 1 << 20] {
        let mut driver = MatchGeneratorDriver::new(8, 1);
        driver.set_source_size_hint(hint);
        driver.reset(level);
        driver.prime_with_dictionary(b"abcdefghABCDEFGHijklmnop", [1, 4, 8]);
        assert!(
            driver.borrowed_dict_supported(),
            "Fast dict frame with hint {hint} must attach (borrowed in-place \
             dict scan), never fall back to the copy-mode input-copy path"
        );
    }
}

#[test]
fn fast_attach_cutoff_keeps_virtual_positions_within_u32() {
    // The cutoff is 31, NOT the full u64 source-size range, because the borrowed
    // dict kernel stores virtual positions as u32 (`cur_abs as u32`). The largest
    // attached source `1 << CUTOFF` (plus the dict prefix) must stay below
    // u32::MAX or that arithmetic wraps; the next bucket (4 GiB) would. This pins
    // the bound so a future "just raise it to attach everything" change cannot
    // silently reintroduce the overflow — raising the cutoff requires widening
    // the kernel's position type first.
    let max_attached: u64 = 1u64 << FAST_ATTACH_DICT_CUTOFF_LOG;
    assert!(
        max_attached <= u32::MAX as u64,
        "the largest attached source 2^{FAST_ATTACH_DICT_CUTOFF_LOG} must fit u32 \
         virtual positions",
    );
    assert!(
        (1u64 << (FAST_ATTACH_DICT_CUTOFF_LOG + 1)) > u32::MAX as u64,
        "the next bucket 2^{} would overflow u32 virtual positions",
        FAST_ATTACH_DICT_CUTOFF_LOG + 1,
    );
}

#[test]
fn oversized_dict_hint_routes_fast_to_copy_mode() {
    // A dict whose region exceeds the tagged attach position field
    // (`MAX_FAST_ATTACH_DICT_REGION`, 16 MiB) must route the Fast prime to COPY
    // mode instead of the tagged attach fill, which would overflow the packed
    // position. The decision is keyed on the load-set size hint, so a hint past
    // the limit suffices to exercise it without allocating a real 16 MiB dict.
    // Copy mode leaves the borrowed in-place dict scan (attach-only) unavailable.
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.set_dictionary_size_hint(MAX_FAST_ATTACH_DICT_REGION + 1);
    driver.reset(CompressionLevel::Level(1));
    driver.prime_with_dictionary(b"small dict content with some padding here", [1, 4, 8]);
    assert!(
        !driver.borrowed_dict_supported(),
        "an oversized dict must use copy mode, not the tagged attach fill"
    );
}

#[test]
fn block_samples_match_dict_is_true_for_non_simple_backend() {
    // Production fallback: a non-Simple backend (here Row, Level 6) has no dict
    // probe, so the driver wrapper answers CONSERVATIVELY `true` for ANY block —
    // keeping the dict frame on the scan rather than letting the raw-fast-path
    // emit a block raw and miss an embedded dict segment (see
    // `dictionary_segment_in_incompressible_input_is_matched`). Only the
    // Simple/Fast backend trades the blanket scan for a precise probe.
    let dict = b"the quick brown fox jumps over the lazy dog 0123456789abcdef";
    let mut row = MatchGeneratorDriver::new(8, 6);
    row.set_dictionary_size_hint(dict.len());
    row.reset(CompressionLevel::Level(6));
    row.prime_with_dictionary(dict, [1, 4, 8]);
    assert!(
        row.block_samples_match_dict(&dict[..32]),
        "non-Simple backend must stay on the scan (true) for a dict frame"
    );
    let random: alloc::vec::Vec<u8> = (0..64u8)
        .map(|i| i.wrapping_mul(37).wrapping_add(13))
        .collect();
    assert!(
        row.block_samples_match_dict(&random),
        "non-Simple backend reports true regardless of block content"
    );
}

#[test]
fn primed_snapshot_fast_attach_does_not_over_key_non_simple_backends() {
    // `fast_attach` is a Simple/Fast-backend concept (the 8 KiB attach-vs-copy
    // table split). Dfast/Row/HashChain each have their OWN attach/copy regime
    // (`DFAST_ATTACH_DICT_CUTOFF_LOG`, `ROW_ATTACH_DICT_CUTOFF_LOG`,
    // `HC_ATTACH_DICT_CUTOFF_LOG`) but those are deliberately kept OUT of the
    // `fast_attach` key, which only models the Fast table split. Their snapshots
    // are keyed by the resolved matcher geometry instead, and the HC modes share
    // one window geometry so an HC cross-mode restore stays decodable (see
    // `prime_with_dictionary`). Either way the `fast_attach`
    // bit must NOT enter a non-Simple snapshot key — otherwise an unhinted
    // capture (which would record `fast_attach = true`) and a hinted reset that
    // resolves to the IDENTICAL `LevelParams` would key differently and force a
    // needless re-prime. `Best` is a Row-backend lazy
    // level; this also pins the Row arm recording its RESOLVED hash width on
    // the unhinted path (a 0 default there keyed unhinted-vs-hinted apart).
    // An explicit Row-backend level: `Best` now sits on level 13 (Btlazy2),
    // so the named alias no longer reaches the Row arm this test pins.
    let mut driver = MatchGeneratorDriver::new(8, 1);
    let level = CompressionLevel::Level(12);

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
        "a Row snapshot must restore across an unhinted vs large-hinted \
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
    // Mirror `row_key_value`: an mls-wide masked key when 8 lookahead bytes
    // exist, the 4-byte key in the tail. `idx = 8` on a 16-byte history has
    // exactly 8 bytes left, so the wide arm applies here.
    let key_len = matcher.mls.min(6);
    let value = u64::from_le_bytes(concat[idx..idx + 8].try_into().unwrap())
        & ((1u64 << (key_len * 8)) - 1);
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
fn hc_hash3_position_matches_hash3_formula() {
    let bytes = [b'a', b'b', b'c', b'd'];
    let read32 = u32::from_le_bytes(bytes);
    let expected = (((read32 << 8).wrapping_mul(HC_PRIME3BYTES)) >> (32 - HC3_HASH_LOG)) as usize;
    assert_eq!(
        super::match_table::storage::MatchTable::hash3_position(&bytes, HC3_HASH_LOG),
        expected
    );
}

#[test]
fn hc_hash_position_matches_hash4_formula() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(HC_CONFIG, super::strategy::StrategyTag::Lazy, 22);
    let bytes = [b'a', b'b', b'c', b'd'];
    let read32 = u32::from_le_bytes(bytes);
    let expected = ((read32.wrapping_mul(HC_PRIME4BYTES)) >> (32 - hc.table.hash_log)) as usize;
    assert_eq!(hc.table.hash_position(&bytes), expected);
}

#[test]
fn btultra2_main_hash_uses_hash4_formula() {
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

#[cfg(feature = "bench_internals")]
pub(crate) fn level22_block_ranges(data: &[u8]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut cursor = 0usize;
    let mut savings = 0i64;
    while cursor < data.len() {
        let remaining = data.len() - cursor;
        let candidate_len = remaining.min(super::cost_model::HC_BLOCKSIZE_MAX);
        let block_len = crate::encoding::frame_compressor::optimal_block_size(
            CompressionLevel::Level(22),
            &data[cursor..cursor + candidate_len],
            remaining,
            super::cost_model::HC_BLOCKSIZE_MAX,
            savings,
        )
        .min(candidate_len)
        .max(1);
        ranges.push((cursor, block_len));
        cursor += block_len;
        // The exact upstream zstd gate uses compressed-size savings. For this corpus
        // parity harness, after the first full block has compressed, savings is
        // sufficient to authorize the same pre-block splitter path.
        if cursor >= super::cost_model::HC_BLOCKSIZE_MAX {
            savings = 3;
        }
    }
    ranges
}

#[cfg(feature = "bench_internals")]
fn merge_block_delimiters(sequences: Vec<(usize, usize, usize)>) -> Vec<(usize, usize, usize)> {
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

/// White-box capture of the level-22 sequence stream (literal-length,
/// offset, match-length triples) the match generator emits for `data`,
/// with block-delimiter pseudo-sequences merged into the following
/// triple's literal run. Pure Rust; the C-conformance comparison that
/// consumes it lives in the `ffi-bench` crate.
#[cfg(feature = "bench_internals")]
pub(crate) fn collect_level22_sequences(data: &[u8]) -> Vec<(usize, usize, usize)> {
    merge_block_delimiters(collect_level22_sequences_with_delimiters(data))
        .into_iter()
        .filter(|(_, offset, match_len)| *offset != 0 || *match_len != 0)
        .collect()
}

#[cfg(feature = "bench_internals")]
fn collect_level22_sequences_with_delimiters(data: &[u8]) -> Vec<(usize, usize, usize)> {
    let mut driver = MatchGeneratorDriver::new(super::cost_model::HC_BLOCKSIZE_MAX, 1);
    driver.set_source_size_hint(data.len() as u64);
    driver.reset(CompressionLevel::Level(22));

    let mut sequences = Vec::new();
    for (chunk_start, chunk_len) in level22_block_ranges(data) {
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
// disabled: tests legacy SuffixStore behavior incompatible with upstream zstd-shape kernel's HASH_READ_SIZE geometry
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
fn resident_reapply_restores_retained_dictionary_budget() {
    // A reused-dict frame that re-borrows the resident dictionary (skips the
    // re-prime) must restore the retained-dict budget the per-frame `reset`
    // cleared. The matcher's `reset` re-inflates `max_window_size` by the dict
    // region; without the restore the driver-level budget stays 0 and
    // `retire_dictionary_budget` never shrinks that inflated window as the dict
    // evicts. For the HashChain backend (whose `window_low` is measured against
    // `max_window_size`) that lets a post-eviction match exceed the frame
    // header's base window and emit an over-window offset.
    let mut driver = MatchGeneratorDriver::new(1 << 16, 1);
    let dict = b"abcdefghABCDEFGHijklmnopqrstuvwxyz0123456789";
    driver.set_dictionary_size_hint(dict.len());
    driver.reset_on_hc_lazy(CompressionLevel::Better);
    driver.prime_with_dictionary(dict, [1, 4, 8]);
    let base = driver.reported_window_size;
    assert!(
        driver.dictionary_retained_budget > 0,
        "the priming frame must retain a non-zero dict budget"
    );

    // Second frame: the reset detects the resident dict and re-borrows it.
    driver.set_dictionary_size_hint(dict.len());
    driver.reset_on_hc_lazy(CompressionLevel::Better);
    assert!(
        driver.dictionary_is_resident(),
        "the second frame must re-borrow the resident dictionary"
    );
    assert_eq!(
        driver.dictionary_retained_budget, 0,
        "reset clears the retained-dict budget"
    );
    let inflated = driver.hc_matcher().table.max_window_size;
    assert!(
        inflated > base,
        "reset re-inflates the window by the resident dict region \
         (inflated={inflated}, base={base})"
    );

    driver.reapply_resident_dictionary([1, 4, 8]);
    assert_eq!(
        driver.dictionary_retained_budget,
        inflated - base,
        "resident reapply must restore the retained-dict budget (= window \
         inflation) so the retire path can shrink the window as the dict evicts"
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
    // Single-slot tables (upstream zstd parity): the bucket holds at most one
    // u32; the assertion below is a direct equality (no `.contains`).
    assert_ne!(
        target_slot, DFAST_EMPTY_SLOT,
        "pack_slot must never return the empty-slot sentinel for a real position"
    );
    assert_eq!(
        matcher.tables[long_hash], target_slot,
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
        matcher.tables[matcher.long_len() + short_hash],
        target_slot,
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
        matcher.tables[matcher.long_len() + short_hash],
        target_slot,
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
    let short0 = dfast.long_len();
    dfast.tables[short0] = early_packed;
    dfast.tables[0] = early_packed;

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
    // upstream zstd parity for `ZSTD_window_reduce`'s clamp-at-zero rule.
    // Verify BOTH tables — `reduce()` walks them in sequence.
    assert_eq!(
        dfast.tables[dfast.long_len()],
        DFAST_EMPTY_SLOT,
        "pre-rebase short-hash entries below the reducer must become empty"
    );
    assert_eq!(
        dfast.tables[0], DFAST_EMPTY_SLOT,
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
    // the matcher's step-skip schedule (upstream zstd-shape kernel walks ip0
    // with kSearchStrength-driven stride growth) — the legacy
    // SuffixStore-based matcher iterated every position and always
    // hit short repeats, but the upstream zstd-shape kernel may skip over
    // them when the step has grown large by the time it reaches the
    // repeat region. The substance of this test is the
    // reconstruction assertion below; `saw_triple` was a legacy
    // tuning preference, not a correctness invariant.
    let _ = saw_triple;
    assert_eq!(rebuilt, data);
}

#[test]
fn fast_levels_dispatch_per_level_hash_log_and_mls() {
    // Level 1 — upstream zstd `{ 19, 13, 14, 1, 7, 0, ZSTD_fast }` row:
    // window_log=19, hash_log=14, mls=7.
    let f1 = resolve_level_params(CompressionLevel::Level(1), None)
        .fast
        .unwrap();
    assert_eq!(f1.hash_log, 14);
    assert_eq!(f1.mls, 7);
    assert_eq!(f1.step_size, 2);

    // Negative levels — upstream zstd row-0 ("base for negative"):
    // hash_log=13, mls=7. The 32 KiB table is L1d-resident (every
    // probe an L1 hit, vs an L2 access for a 64 KiB hash_log=14
    // table), and minMatch=7 drops short-distance 6-byte matches —
    // upstream zstd parity on both ratio and throughput.
    // step_size follows upstream zstd's formula: targetLength = -level,
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
    // tuning; not part of the negative-level upstream zstd ladder).
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
/// Asserts against the constant `clevels.h` table[0] `targetLength` column
/// (transcribed inline) — a pure-Rust in-tree test, no FFI dependency.
#[test]
fn lazy_band_target_len_matches_default_table() {
    // table[0] (srcSize > 256 KB) targetLength, levels 5..=15: the lazy
    // outer loop's nice-match (`sufficient_len`) threshold.
    let expected: [(i32, usize); 11] = [
        (5, 2),
        (6, 4),
        (7, 8),
        (8, 16),
        (9, 16),
        (10, 16),
        (11, 16),
        (12, 32),
        (13, 32),
        (14, 32),
        (15, 32),
    ];
    for (level, want) in expected {
        let params = resolve_level_params(CompressionLevel::Level(level), None);
        // L5 = greedy (Row backend → `row`); L6-15 = lazy (HashChain → `hc`).
        let target_len = params
            .hc
            .map(|hc| hc.target_len)
            .or_else(|| params.row.map(|row| row.target_len))
            .expect("lazy/greedy level carries hc or row config");
        assert_eq!(target_len, want, "L{level}: target_len must match table[0]");
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
fn upper_lazy_band_params_match_default_table() {
    // table[0] (srcSize > 256 KB), levels 13..=15 (btlazy2 budget):
    // (level, windowLog, hashLog, chainLog, search_depth = 1 << searchLog).
    let expected: [(i32, u8, usize, usize, usize); 3] = [
        (13, 22, 22, 22, 1 << 4),
        (14, 22, 23, 22, 1 << 5),
        (15, 22, 23, 23, 1 << 6),
    ];
    for (level, wlog, hlog, clog, sd) in expected {
        let params = resolve_level_params(CompressionLevel::Level(level), None);
        let hc = params.hc.unwrap();
        assert_eq!(hc.search_depth, sd, "L{level}: search_depth");
        assert_eq!(params.window_log, wlog, "L{level}: window_log");
        assert_eq!(hc.hash_log, hlog, "L{level}: hash_log");
        assert_eq!(hc.chain_log, clog, "L{level}: chain_log");
    }
}
