//! Matching algorithm used find repeated parts in the original data
//!
//! The Zstd format relies on finden repeated sequences of data and compressing these sequences as instructions to the decoder.
//! A sequence basically tells the decoder "Go back X bytes and copy Y bytes to the end of your decode buffer".
//!
//! The task here is to efficiently find matches in the already encoded data for the current suffix of the not yet encoded data.

#[cfg(feature = "bench-internals")]
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
use super::simple::fast_matcher::{
    FAST_LEVEL_1_HASH_LOG, FAST_LEVEL_1_MLS, FastKernelMatcher, TableCarry,
};
use super::workspace::HistoryBuf;

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
// `kSearchStrength` (upstream `zstd_compress_internal.h:32`). The dfast step
// ramp grows one position every `1 << kSearchStrength` = 256 bytes travelled
// (upstream `kStepIncr`, zstd_double_fast.c:131). A smaller value accelerates
// the scan faster and skips source positions upstream still inserts, which
// drops the short matches upstream finds at a block start — so the
// `#167`-disabled path must use the upstream 8 to stay byte-identical.
pub(crate) const DFAST_SKIP_SEARCH_STRENGTH: usize = 8;
pub(crate) const DFAST_SKIP_STEP_GROWTH_INTERVAL: usize = 1 << DFAST_SKIP_SEARCH_STRENGTH;
/// How densely dfast indexes a block it wrote off without searching.
///
/// The block is not searched, so the only reason to index it at all is that a
/// LATER block may duplicate it, and a duplicate is block-sized: the search
/// that scans it meets an indexed position within one step, which is far inside
/// what it scans anyway. Indexing at every sixteenth position instead cost a
/// third of the encode on incompressible input — two tables, sixty-five
/// thousand stores per mebibyte — for a proximity nothing needs. Fast reads the
/// same reasoning from
/// [`RAW_SKIP_INDEX_STEP`](crate::encoding::incompressible::RAW_SKIP_INDEX_STEP).
pub(crate) const DFAST_INCOMPRESSIBLE_SKIP_STEP: usize =
    crate::encoding::incompressible::RAW_SKIP_INDEX_STEP;
pub(crate) const ROW_HASH_BITS: usize = 20;
pub(crate) const ROW_LOG: usize = 5;
pub(crate) const ROW_SEARCH_DEPTH: usize = 16;
pub(crate) const ROW_TARGET_LEN: usize = 48;
pub(crate) const ROW_TAG_BITS: usize = 8;
pub(crate) const ROW_EMPTY_SLOT: u32 = u32::MAX;
pub(crate) const ROW_HASH_KEY_LEN: usize = 4;
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

// Dictionary prime + CDict-equivalent snapshot lifecycle. A child module so it
// can reach the driver's private `primed` / `reset_shape` state directly; the
// `Matcher` trait's dict entry points forward to its inherent `*_impl` helpers.
mod dict_prime;

// `Strategy` and `StrategyTag` live in `crate::encoding::strategy`.
// The driver carries a `StrategyTag` field set at `reset()` and
// dispatches each block into a monomorphised `compress_block::<S>`
// per concrete strategy.

/// Backend storage for [`MatchGeneratorDriver`]. Exactly one match-finder
/// state lives in the driver at a time — the active variant. Backend
/// transitions in [`Matcher::reset`] release the current variant's tables and
/// then replace `storage` with a freshly constructed variant for the new
/// backend.
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
///
/// The variant is chosen by the resolved search method,
/// [`LevelParams::backend`](crate::encoding::levels::config::LevelParams::backend),
/// never by the level number alone: the search method comes from the
/// (level, source size) cParams row plus any parameter override, and the
/// size tier can change it. Level 11, for one, runs lazy2 on the Row backend
/// for a large source and btopt on the HashChain backend for 16 KiB or less.
/// Resolve the parameters for the input before assuming which backend runs.
#[derive(Clone)]
enum MatcherStorage {
    /// Upstream zstd `ZSTD_fast` family, for `SearchMethod::Fast`.
    /// Constructed by [`MatchGeneratorDriver::new`] as the initial variant
    /// and re-selected by [`Matcher::reset`] when that search method resolves.
    Simple(FastKernelMatcher),
    /// Upstream zstd `ZSTD_dfast` family — two-table hash chain, for
    /// `SearchMethod::DoubleFast`.
    Dfast(DfastMatchGenerator),
    /// Upstream zstd `lazy_generic` parse (`ZSTD_greedy` / `ZSTD_lazy` /
    /// `ZSTD_lazy2` / `ZSTD_btlazy2`), for `SearchMethod::RowHash` and
    /// `SearchMethod::BinaryTreeLazy`. The Row backend itself searches a hash
    /// chain instead of rows when the resolved window is small.
    Row(RowMatchGenerator),
    /// The hash-chain matcher, for `SearchMethod::HashChain` and
    /// `SearchMethod::BinaryTree`; the latter carries the BT-based optimal
    /// modes (`btopt` / `btultra` / `btultra2`). The [`HcMatchGenerator`]'s
    /// internal [`HcBackend`] discriminator decides whether BT scratch is
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

    /// Capacity of the buffer blocks are read into, for the backends that
    /// ingest in place. A frame sized up front reserves this exactly; one that
    /// grew into it lands on a doubling step instead, which is what makes the
    /// difference observable to a test.
    #[cfg(test)]
    fn ingest_capacity(&self) -> usize {
        match self {
            Self::Simple(m) => m.history_capacity(),
            Self::Dfast(m) => m.history.capacity(),
            Self::Row(m) => m.history.capacity(),
            Self::HashChain(m) => m.table.history.capacity(),
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
    /// Active match-finder state. Exactly one backend lives here at a
    /// time; [`Matcher::reset`] swaps in a freshly constructed variant for
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
    /// Chunk a dictionary is primed in: `base_slice_size` capped by the
    /// frame's window.
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
    // Dictionary sizes for the next frame (set via set_dictionary_size_hint,
    // consumed on reset): the serialized size keys the CDict cParams tier the
    // frame runs (upstream `ZSTD_getCParamRowSize` on `dictSize`), the content
    // size the dictionary tables and attach cutoffs.
    dictionary_size_hint: Option<super::DictionarySizes>,
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
    /// Where the tables live when the driver is reset on its own through
    /// [`Matcher::reset`]; inside a compression context they live in the
    /// context's workspace instead and this stays empty.
    own_workspace: crate::encoding::workspace::Workspace,
    /// Whether this frame's input, handed over as one slice, is scanned in
    /// place rather than copied into the history. Decided at the reset,
    /// because it decides how large the history is laid out.
    frame_in_place: bool,
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

/// Whether a configured HashChain matcher attaches the dictionary for a frame
/// of `size_log` (see [`MatchGeneratorDriver::hc_dict_attach_mode`]).
fn hc_attaches_dictionary(hc: &HcMatchGenerator, size_log: Option<u8>) -> bool {
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
    size_log.is_none_or(|log| log <= cutoff)
}

/// History bytes a frame lays out: the dictionary primed at its head and the
/// input it is expected to bring. Input that can fill the window slides it, and
/// the history then grows to the most it ever holds, which is laid out from the
/// start, as upstream sizes its input buffer from the window: for the Fast
/// backend twice the window grown by the dictionary (it drains back to one
/// window when an append would pass two), for the others that window plus the
/// quarter of it compaction leaves behind; either way plus one pending block of
/// the frame's size.
///
/// A size known exactly (a slice, or a pledge the context enforces) is taken as
/// it is, and its reads are held to what remains, so nothing past it is ever
/// asked for. A size hint on a stream is a claim about data not yet read,
/// trusted only up to the window the LEVEL would choose for it, and a read may
/// find more than it said, so it keeps one block of slack; overriding the
/// window is a claim of its own that only the data can confirm. An unknown size
/// may fill any window, and is laid out for it up front, as upstream's buffered
/// stream takes `windowSize + blockSize` of input buffer for an unknown pledge
/// (`ZSTD_resetCCtx_internal`, `zstd_compress.c:2131-2139`). Room no input
/// reaches is never written, so it is never resident: a one-byte stream through
/// the CLI at levels 3, 19 and 22 peaks at the same RSS as a history grown by
/// reading, and growing it instead measured no faster.
fn frame_history_bytes(
    backend: super::strategy::BackendTag,
    workspace: &crate::encoding::workspace::Workspace,
    in_place: bool,
    hint: Option<u64>,
    dict_len: usize,
    max_window_size: usize,
    level: CompressionLevel,
) -> usize {
    use super::match_table::storage::MAX_PRIMED_WINDOW_SIZE;
    use crate::encoding::workspace::IngestPlan;
    let block = workspace.block_for_window(max_window_size);
    // `MAX_PRIMED_WINDOW_SIZE` is `(u32::MAX - MAX_BLOCK_SIZE) / 2`, so either
    // ceiling stays inside `usize` on a 32-bit target.
    let window = max_window_size
        .checked_add(dict_len)
        .map_or(MAX_PRIMED_WINDOW_SIZE, |w| w.min(MAX_PRIMED_WINDOW_SIZE));
    let slack = match backend {
        super::strategy::BackendTag::Simple => window,
        _ => window >> 2,
    };
    let ceiling = window + slack + block;
    // A size past `usize` is past the ceiling too.
    let bytes = hint.map(|bytes| usize::try_from(bytes).unwrap_or(usize::MAX));
    // The input and the room past it its last read may ask for.
    let (input, read_slack) = match workspace.ingest() {
        IngestPlan::Raw => return 0,
        IngestPlan::Slice(_) if in_place => return dict_len.min(ceiling),
        IngestPlan::Slice(len) | IngestPlan::PledgedStream(len) => (Some(len), 0),
        IngestPlan::Stream => (
            bytes.map(|bytes| {
                let level_window_log =
                    crate::encoding::levels::config::resolve_level_params(level, hint).window_log;
                bytes.min(1usize << level_window_log)
            }),
            block,
        ),
    };
    match input {
        // A window stays below 2^31 and a block below 2^17, so
        // `bytes + read_slack` fits; a dictionary large enough to overflow the
        // rest is past the ceiling anyway.
        Some(bytes) if bytes < max_window_size => dict_len
            .checked_add(bytes + read_slack)
            .map_or(ceiling, |sum| sum.min(ceiling)),
        _ => ceiling,
    }
}

impl MatchGeneratorDriver {
    /// See [`MatcherStorage::ingest_capacity`].
    #[cfg(test)]
    pub(crate) fn ingest_capacity(&self) -> usize {
        self.storage.ingest_capacity()
    }

    /// `(tables, history)` bytes the active window backend holds in
    /// allocations of its own rather than in a context's workspace.
    #[cfg(test)]
    pub(crate) fn owned_table_and_history_bytes(&self) -> (usize, usize) {
        match &self.storage {
            MatcherStorage::Simple(_) => panic!("asked of a window backend only"),
            MatcherStorage::Dfast(m) => (m.tables.owned_bytes(), m.history.owned_bytes()),
            MatcherStorage::Row(m) => (m.tables.owned_bytes(), m.history.owned_bytes()),
            MatcherStorage::HashChain(m) => {
                (m.table.tables.owned_bytes(), m.table.history.owned_bytes())
            }
        }
    }

    /// Read `input` in and commit it as one block, the way the frame loop
    /// does in two calls.
    #[cfg(any(test, feature = "bench-internals"))]
    pub(crate) fn commit_input(&mut self, input: &[u8]) {
        self.fill_in_place(input.len(), &mut |history| {
            history.extend_from_slice(input);
            (input.len(), false)
        });
        self.commit_filled(input.len());
    }

    /// `slice_size` sets the chunk a dictionary is primed in.
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
            own_workspace: crate::encoding::workspace::Workspace::new(),
            frame_in_place: false,
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
        use super::strategy::{BackendTag, SearchMethod};
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
            BackendTag::HashChain => matches!(self.search, SearchMethod::HashChain),
        }
    }

    /// Whether this frame, handed over as one slice with no dictionary, is
    /// scanned in place. Settled by the reset, which laid the history out for
    /// it, so the frame loop must take the path this names.
    pub(crate) fn frame_scans_in_place(&self) -> bool {
        self.frame_in_place
    }

    /// Whether a borrowed scan starting now sees nothing of an earlier frame,
    /// so the frame is the one a fresh matcher writes. Asked once per frame,
    /// after `reset`; [`Self::borrowed_supported`] stays the per-block
    /// invariant. Only the Dfast kernel numbers a borrowed frame's input from
    /// zero regardless of what its tables hold; the others advance their
    /// floor past the previous frame, borrowed or not.
    pub(crate) fn borrowed_frame_is_independent(&self) -> bool {
        match &self.storage {
            MatcherStorage::Dfast(d) => !d.tables_hold_earlier_frames,
            _ => true,
        }
    }

    /// Make [`Self::borrowed_frame_is_independent`] hold by emptying the
    /// tables of earlier frames.
    pub(crate) fn forget_earlier_frames(&mut self) {
        if let MatcherStorage::Dfast(d) = &mut self.storage {
            d.forget_earlier_frames();
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
    /// borrowed frame path uses in place of `commit_filled`. While
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
                // The Fast backend drains the oldest bytes in place and
                // reports the count; the window backends evict whole blocks,
                // so their count is the `window_size` delta.
                super::strategy::BackendTag::Simple => {
                    let MatcherStorage::Simple(m) = &mut self.storage else {
                        unreachable!("active_backend() == Simple proven above");
                    };
                    evicted_bytes += m.trim_to_window();
                }
                super::strategy::BackendTag::Dfast => {
                    let dfast = self.dfast_matcher_mut();
                    let pre = dfast.window_size;
                    dfast.trim_to_window();
                    evicted_bytes += pre - dfast.window_size;
                }
                super::strategy::BackendTag::Row => {
                    let row = self.row_matcher_mut();
                    let pre = row.window_size;
                    row.trim_to_window();
                    evicted_bytes += pre - row.window_size;
                }
                super::strategy::BackendTag::HashChain => {
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
        hc_attaches_dictionary(hc, self.reset_size_log)
    }

    fn skip_matching_for_dictionary_priming(&mut self, dict_len: usize) {
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
                    self.simple_mut().skip_matching_for_dict_prime(dict_len);
                } else {
                    self.simple_mut().skip_matching_for_dict_copy();
                }
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
                // The tagged dictionary slots index at most
                // `DFAST_ATTACH_DICT_MAX_LEN` bytes; a larger dictionary is
                // copied into the live tables instead.
                let attach = dict_len <= crate::encoding::dfast::DFAST_ATTACH_DICT_MAX_LEN
                    && self
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
                // The attach / copy decision was made with the CDict's cParams
                // at `reset` (`RowDictPlan`); the backend indexes the dictionary
                // block accordingly.
                self.row_matcher_mut().prime_dictionary_current_block();
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

    fn set_dictionary_size_hint(&mut self, sizes: super::DictionarySizes) {
        self.dictionary_size_hint = Some(sizes);
    }

    /// Dict-relevance gate for the raw-fast-path. Reached only when a dictionary
    /// is active (the caller short-circuits on `dict_active`), so this answers
    /// "could the dict compress this otherwise-incompressible-looking block?".
    /// The Simple (Fast) backend samples its dict table precisely
    /// (`FastKernelMatcher::block_samples_match_dict`); the other backends
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
    /// primed-dictionary snapshot (a cloned backend kept for CDict-equivalent
    /// reuse), and the workspace its tables live in when it is reset on its
    /// own. Tables laid out in a compression context's workspace are the
    /// context's to count. The inline struct itself is accounted by the
    /// owner's `size_of`.
    fn heap_size(&self) -> usize {
        let snapshot = self
            .primed
            .as_ref()
            .map_or(0, |(storage, _, _)| storage.heap_size());
        self.storage.heap_size() + snapshot + self.own_workspace.heap_bytes()
    }

    fn clear_param_overrides(&mut self) {
        self.param_overrides = None;
    }

    fn reset(&mut self, level: CompressionLevel) {
        // On its own the driver lays its tables out in a workspace of its own,
        // which is moved out for the call and back: moving it leaves the
        // allocation, and so every region carved from it, where it is.
        // Driven directly, it takes blocks of any size up to the format's, so
        // that is the block its history leaves room for; it carves no block
        // buffers of its own.
        let mut own = core::mem::take(&mut self.own_workspace);
        own.begin_layout(
            crate::common::MAX_BLOCK_SIZE as usize,
            crate::encoding::workspace::no_trailing,
            crate::encoding::workspace::IngestPlan::Stream,
        );
        self.reset_in_workspace(level, &mut own);
        own.release_retired();
        self.own_workspace = own;
    }

    fn reset_in_workspace(
        &mut self,
        level: CompressionLevel,
        workspace: &mut crate::encoding::workspace::Workspace,
    ) {
        let hint = self.source_size_hint.take();
        // An empty dictionary is "no dictionary": it primes nothing, so every
        // dictionary-frame decision below must see `None` for it.
        let dict_hint = self
            .dictionary_size_hint
            .take()
            .filter(|sizes| sizes.content > 0);
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
            dict_hint.is_none_or(|sizes| sizes.content <= MAX_FAST_ATTACH_DICT_REGION);
        let hinted = hint.is_some();
        // A dictionary frame takes its cParams and match-finder from the
        // CDict's cParams (upstream `ZSTD_resetCCtx_usingCDict`), whose tier
        // is keyed by the serialized dictionary size; a lazy-band CDict also
        // carries `dict_plan` to the Row backend. The dictionary is prepared
        // under the caller's parameters, so they are part of those cParams.
        let overrides = self.param_overrides.unwrap_or_default();
        // A dictionary frame runs the shape the dictionary was prepared with,
        // whatever the source size. Upstream stops doing that once the source
        // outgrows the dictionary and resolves the frame's own instead
        // (zstd_compress.c:5264). That alternative was implemented and measured
        // against this one and against the reference, in one session with the
        // arms interleaved, over five levels and two dictionary shapes, and it
        // loses on every row where the two differ. Time per frame over a 1 MiB
        // source, ours / the alternative / the reference:
        //
        //   3 KB dictionary        110 KB dictionary
        //   L1  1.13 / 1.13 / 0.73  1.14 / 1.15 / 0.79 ms
        //   L3  0.24 / 0.30 / 0.27  0.26 / 0.32 / 0.42 ms
        //   L5  0.52 / 2.69 / 0.38  0.43 / 2.77 / 1.16 ms
        //   L9  0.50 / 5.36 / 1.37  0.71 / 5.58 / 2.28 ms
        //   L12 0.87 / 5.56 / 1.87  1.60 / 5.58 / 3.08 ms
        //
        // Three to eight times the time for bytes that match to within a
        // percent, and under the larger dictionary this shape also beats the
        // reference on BOTH axes from L3 up (at L9, 0.71 ms and 243 bytes
        // against 2.28 ms and 1241), which the alternative would have given
        // away. A control arm at 64 KiB, under the size where the alternative
        // can run at all, stayed within 2.4% with identical output.
        //
        // The bench host is a VMware guest with no PMU passthrough, so cycles
        // and instructions are unavailable there (`perf stat -e cycles` reports
        // the event as unsupported); the figures above are `task-clock`, and at
        // this margin an instruction count would not be what decides it.
        //
        // See `dictionary_describes_frame`, which the C ABI still reads for a
        // question the codec cannot answer.
        let (params, dict_plan) = match dict_hint {
            Some(sizes) => crate::encoding::levels::config::resolve_level_params_with_dict(
                level, hint, sizes, &overrides,
            ),
            None => (Self::level_params(level, hint), None),
        };
        #[cfg_attr(not(test), allow(unused_mut))]
        let mut params = params;
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
                SearchMethod::RowHash | SearchMethod::BinaryTreeLazy => {
                    let row = params.row.get_or_insert(ROW_CONFIG);
                    row.bt = matches!(search, SearchMethod::BinaryTreeLazy);
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
        // byte-identical to plain level-based compression. Shared with the
        // workspace estimate, which has to build what this builds.
        if let Some(ov) = self.param_overrides {
            crate::encoding::levels::config::apply_frame_overrides(
                &mut params,
                &ov,
                dict_hint.is_some(),
                hint,
            );
        }
        // A dictionary frame's hash-chain / binary-tree widths are the CDict's
        // (`resolve_level_params_with_dict`): verbatim when the dictionary is
        // copied into the live tables (`ZSTD_resetCCtx_byCopyingCDict` builds
        // the context from the CDict's cParams), re-adjusted to the source
        // alone when it is attached (`byAttachingCDict`: the live tables hold
        // no dictionary entry, the dms carries its own dict-sized tables), so
        // a small source under a large attached dictionary keeps small live
        // tables. Nothing re-sizes `params.hc` here.
        // Upstream `ZSTD_resolveRowMatchFinderMode` (zstd_compress.c:238): the
        // greedy/lazy/lazy2 band searches rows only above a 2^14 window and a
        // hash chain otherwise. The Row backend runs that switch itself
        // (`RowMatchGenerator::use_chain`), so both finders share ONE parse
        // (upstream's single `lazy_generic`) and the level stays on `RowHash`.
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
                    m.tables = crate::encoding::workspace::Table::empty();
                    m.reset();
                }
                MatcherStorage::Row(m) => {
                    // One buffer holds the positions and, in its byte tail,
                    // the cursors and tags — releasing it releases all three.
                    m.release_tables();
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
                    m.table.tables = crate::encoding::workspace::Table::empty();
                    m.table.chain_off = 0;
                    m.table.hash3_off = 0;
                    m.reset();
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
                    // Deferred: the reset below lays the table out in the
                    // workspace at the frame's resolved width.
                    MatcherStorage::Simple(FastKernelMatcher::with_params_deferred(
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
        // The Fast backend's dictionary mode, which `prime_with_dictionary`
        // takes from the same `reset_size_log`: attached, the dictionary goes
        // into a table of its own and the input is scanned in place.
        let fast_attach = matches!(next_backend, super::strategy::BackendTag::Simple)
            && self.reset_dict_attach_ok
            && self
                .reset_size_log
                .is_none_or(|log| log <= FAST_ATTACH_DICT_CUTOFF_LOG);
        // A slice on a backend that can scan it in place never enters the
        // history, with no dictionary or one the Fast backend attaches; the frame
        // loop reads this decision back (`frame_scans_in_place`) instead of
        // taking it again. The kernels keep positions in `u32`, counting a
        // dictionary ahead of the slice, so a longer one is copied.
        let dict_len = dict_hint.map_or(0, |sizes| sizes.content);
        self.frame_in_place = match workspace.ingest() {
            crate::encoding::workspace::IngestPlan::Slice(len) => {
                (dict_hint.is_none() || fast_attach)
                    && self.borrowed_supported()
                    && len
                        .checked_add(dict_len)
                        .is_some_and(|len| len <= u32::MAX as usize)
            }
            _ => false,
        };
        let history_bytes = frame_history_bytes(
            next_backend,
            workspace,
            self.frame_in_place,
            hint,
            dict_hint.map_or(0, |sizes| sizes.content),
            max_window_size,
            level,
        );
        // The input the frame can write into its tables: the size when known,
        // otherwise the most its history is laid out to hold.
        let expected_input = hint.map_or(history_bytes, |bytes| {
            usize::try_from(bytes).unwrap_or(usize::MAX)
        });
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
                let dict_attach_epoch = dict_hint.is_some()
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
                let table_overwritten_by_restore = dict_hint.is_some()
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
                let carry = if table_overwritten_by_restore {
                    TableCarry::OverwrittenByRestore
                } else if dict_attach_epoch {
                    TableCarry::AdvanceEpoch
                } else {
                    TableCarry::Clear
                };
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
                let hash_log = if dict_hint.is_some() {
                    fast.hash_log
                } else {
                    fast.hash_log.min(params.window_log as u32 + 1)
                };
                // The attached dictionary table takes the CDict's `hashLog`
                // (upstream `ZSTD_createCDict`: `ZSTD_getCParams(level,
                // UNKNOWN, dictSize)` adjusted for a `minSrcSize` source), not
                // the source-capped main width: a small source must not
                // shrink the table a large dictionary was sized for.
                m.set_dict_table_hash_log(dict_hint.map(|sizes| {
                    crate::encoding::cparams::get_cdict_cparams(
                        crate::encoding::levels::config::numeric_level(level),
                        sizes.serialized,
                        &overrides,
                    )
                    .hash_log
                }));
                let tables =
                    crate::encoding::simple::fast_kernel::hash_table::FastHashTable::workspace_bytes(
                        hash_log,
                    );
                // The history keeps only what the reset will, as on the dfast arm.
                m.retire_history();
                workspace.open_for_match_finder(
                    tables + m.history_workspace_bytes(history_bytes),
                    tables,
                    max_window_size,
                    expected_input,
                );
                // The history binds first, carrying its bytes out from under
                // where the table may now land.
                m.bind_history(workspace, history_bytes);
                m.reset(
                    params.window_log,
                    hash_log,
                    fast.mls,
                    fast.step_size,
                    carry,
                    workspace,
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
                // The attached dictionary tables take the CDict's geometry
                // (upstream hashes the dictMatchState tables with
                // `dictCParams`), not the source-capped live widths.
                dfast.set_dict_table_bits(dict_hint.map(|sizes| {
                    let cd = crate::encoding::cparams::get_cdict_cparams(
                        crate::encoding::levels::config::numeric_level(level),
                        sizes.serialized,
                        &overrides,
                    );
                    (cd.hash_log as usize, cd.chain_log as usize)
                }));
                // A copy-mode frame (source past the attach cutoff, or a
                // dictionary too large to tag) merges the dictionary into the
                // live tables; a resident attach-mode table must not be
                // re-borrowed under it (same decision as the prime dispatch).
                // The width-change invalidation in `set_hash_bits` does not
                // cover an unhinted attach frame followed by a large hinted
                // one whose live widths coincide at the level's full widths.
                let dfast_attach_next = dict_hint.is_some_and(|sizes| {
                    sizes.content <= crate::encoding::dfast::DFAST_ATTACH_DICT_MAX_LEN
                }) && self
                    .reset_size_log
                    .is_none_or(|log| log <= DFAST_ATTACH_DICT_CUTOFF_LOG);
                if dict_hint.is_some() && !dfast_attach_next {
                    dfast.invalidate_dict_cache();
                }
                // The widths are settled, so the tables can be laid out; the
                // reset below reads whether they continue the last frame's.
                // The history binds first, carrying its bytes out from under
                // where the tables may now land.
                let tables = dfast.tables_workspace_bytes();
                // The history keeps only what the reset will: laid out and
                // carried over at its full length, a long stream's window would
                // be moved into room the next frame never reads.
                dfast.retire_history();
                workspace.open_for_match_finder(
                    tables + dfast.history.workspace_bytes(history_bytes),
                    tables,
                    max_window_size,
                    expected_input,
                );
                dfast.history.bind(workspace, history_bytes);
                dfast.bind_tables(workspace);
                dfast.reset();
            }
            MatcherStorage::Row(row) => {
                row.max_window_size = max_window_size;
                row.lazy_depth = params.lazy_depth;
                row.set_dict_plan(dict_plan);
                let mut row_cfg = params.row.expect("Row level row carries a RowConfig");
                // A dictionary frame's widths already come from the CDict's
                // cParams (adjusted to the source when attached); only a plain
                // hinted frame caps them by the window here.
                if hinted && dict_plan.is_none() {
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
                // The finder and its widths are settled by `configure`, so the
                // tables can be laid out; the reset reads whether they continue
                // the last frame's. The history keeps only what the reset can,
                // as on the dfast arm.
                row.retire_history();
                workspace.open_for_match_finder(
                    row.tables_workspace_bytes() + row.history.workspace_bytes(history_bytes),
                    row.zero_table_bytes(),
                    max_window_size,
                    expected_input,
                );
                row.history.bind(workspace, history_bytes);
                row.bind_tables(workspace);
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
                // Skip for dictionary frames: their `hc_cfg.{hash,chain}_log`
                // are the CDict's (verbatim when the dictionary is copied
                // into the live tables, source-adjusted when it is attached);
                // re-applying the source-window cap would collapse a copied
                // dictionary's tables to the small hinted source.
                if hinted && dict_hint.is_none() {
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
                // A copy-mode frame merges the dictionary into the live tables,
                // so the previous attach frame's dms must not be re-borrowed
                // under it (the same decision the prime makes, taken here
                // because the reset below re-borrows on a primed dms alone).
                if dict_hint.is_some() && !hc_attaches_dictionary(hc, self.reset_size_log) {
                    hc.table.dms.invalidate();
                }
                // The widths are settled by `configure`, so the tables can be
                // laid out before the reset retires the previous frame's
                // entries in them.
                let tables = hc.table.tables_workspace_bytes();
                // The history keeps only what the reset will, as on the dfast arm.
                hc.table.retire_history();
                workspace.open_for_match_finder(
                    tables + hc.table.history.workspace_bytes(history_bytes),
                    tables,
                    max_window_size,
                    expected_input,
                );
                hc.table.history.bind(workspace, history_bytes);
                hc.table.bind_tables(workspace);
                hc.reset();
            }
        }
        // LDM wiring (#27): attach (or clear) the long-distance-match
        // producer on the optimal (BT) backend. LDM is the only
        // back-reference path that crosses the regular window, so it
        // only has a home on the `BtMatcher`; non-BT strategies drop the
        // producer. Built AFTER `hc.reset()` because `BtMatcher::reset`
        // clears an existing producer's table but does not null the
        // slot — installing here gives the new frame a fresh producer.
        #[cfg(feature = "ldm")]
        {
            // Resolve the derived LDM params first (immutable borrow of the
            // overrides), then reuse the existing producer's allocation below.
            let derived_ldm = self
                .param_overrides
                .as_ref()
                .and_then(|ov| ov.ldm)
                .map(|ldm_ov| crate::encoding::levels::config::frame_ldm_params(&params, &ldm_ov));
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
        // key would over-key identical resolved shapes. `fast_attach` is the
        // decision `prime_with_dictionary` makes from the same `reset_size_log`.
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
        // Everything is laid out in `workspace` now. A driver reset on its own
        // before it joined a context still holds the workspace it used then,
        // which nothing points into any more; on its own the field is empty
        // here, since `reset` has it out for the call.
        self.own_workspace = crate::encoding::workspace::Workspace::new();
    }

    // Dictionary entry points forward to the `dict_prime` child module, which
    // owns the prime / snapshot lifecycle (it reaches the driver's private
    // `primed` / `reset_shape` state directly as a descendant module).

    #[inline]
    fn dictionary_is_resident(&self) -> bool {
        self.dictionary_is_resident_impl()
    }

    #[inline]
    fn reapply_resident_dictionary(&mut self, offset_hist: [u32; 3]) {
        self.reapply_resident_dictionary_impl(offset_hist)
    }

    #[inline]
    fn prime_with_dictionary(&mut self, dict_content: &[u8], offset_hist: [u32; 3]) {
        self.prime_with_dictionary_impl(dict_content, offset_hist)
    }

    #[inline]
    fn restore_primed_dictionary(&mut self, level: super::CompressionLevel) -> bool {
        self.restore_primed_dictionary_impl(level)
    }

    #[inline]
    fn capture_primed_dictionary(&mut self, level: super::CompressionLevel) {
        self.capture_primed_dictionary_impl(level)
    }

    #[inline]
    fn invalidate_primed_dictionary(&mut self) {
        self.invalidate_primed_dictionary_impl()
    }

    #[inline]
    fn seed_dictionary_entropy(
        &mut self,
        huff: Option<&crate::huff0::huff0_encoder::HuffmanTable>,
        ll: Option<&crate::fse::fse_encoder::FSETable>,
        ml: Option<&crate::fse::fse_encoder::FSETable>,
        of: Option<&crate::fse::fse_encoder::FSETable>,
    ) {
        self.seed_dictionary_entropy_impl(huff, ll, ml, of)
    }

    fn leave_workspace(&mut self) {
        match &mut self.storage {
            MatcherStorage::Simple(m) => m.leave_workspace(),
            MatcherStorage::Dfast(m) => {
                m.tables.leave_workspace();
                m.history.leave_workspace();
            }
            MatcherStorage::Row(m) => {
                m.tables.leave_workspace();
                m.history.leave_workspace();
            }
            MatcherStorage::HashChain(m) => {
                m.table.tables.leave_workspace();
                m.table.history.leave_workspace();
            }
        }
    }

    fn window_size(&self) -> u64 {
        self.reported_window_size as u64
    }

    fn get_last_space(&mut self) -> &[u8] {
        match &self.storage {
            MatcherStorage::Simple(m) => m.last_committed_space(),
            MatcherStorage::Dfast(m) => m.get_last_space(),
            MatcherStorage::Row(m) => m.get_last_space(),
            MatcherStorage::HashChain(m) => m.table.get_last_space(),
        }
    }

    /// Read the next block STRAIGHT into the backend's history buffer.
    ///
    /// The bytes are in the buffer but not yet part of the window: the caller
    /// picks the block boundary from [`Self::uncommitted_input`] and then
    /// calls [`Self::commit_filled`].
    fn fill_in_place(
        &mut self,
        capacity: usize,
        fill: &mut dyn FnMut(&mut HistoryBuf) -> (usize, bool),
    ) -> (usize, bool) {
        match &mut self.storage {
            MatcherStorage::Simple(m) => m.fill_uncommitted(capacity, fill),
            MatcherStorage::Dfast(m) => m.fill_uncommitted(capacity, fill),
            MatcherStorage::Row(m) => m.fill_uncommitted(capacity, fill),
            MatcherStorage::HashChain(m) => m.table.fill_uncommitted(capacity, fill),
        }
    }

    /// Bytes read by [`Self::fill_in_place`] that no block has claimed yet.
    fn uncommitted_input(&self) -> &[u8] {
        match &self.storage {
            MatcherStorage::Simple(m) => m.uncommitted(),
            MatcherStorage::Dfast(m) => m.uncommitted(),
            MatcherStorage::Row(m) => m.uncommitted(),
            MatcherStorage::HashChain(m) => m.table.uncommitted(),
        }
    }

    /// Claim `len` bytes of [`Self::uncommitted_input`] as the next block.
    ///
    /// A dictionary inflates `max_window_size` so the primed bytes stay
    /// reachable, and once eviction carries them out of the window that
    /// inflation has to be retired. Skipping it leaves the backend admitting
    /// matches older than the window the frame header reports, which encodes
    /// an offset no decoder can resolve.
    fn commit_filled(&mut self, len: usize) {
        // The window backends evict whole blocks inside `commit_block`,
        // subtracting each evicted block from `window_size` and then adding
        // `len`, so `pre + len - post` is exactly what was evicted and never
        // negative; the sum is two byte counts bounded by the window. The Fast
        // backend reports the bytes it drained itself.
        let evicted_bytes = match &mut self.storage {
            MatcherStorage::Simple(m) => m.commit_block(len),
            MatcherStorage::Dfast(m) => {
                let pre = m.window_size;
                m.commit_block(len);
                pre + len - m.window_size
            }
            MatcherStorage::Row(m) => {
                let pre = m.window_size;
                m.commit_block(len);
                pre + len - m.window_size
            }
            MatcherStorage::HashChain(m) => {
                let pre = m.table.window_size;
                m.table.commit_block(len);
                pre + len - m.table.window_size
            }
        };
        // The second trim pass only does work when retiring the budget shrank
        // `max_window_size` below the backend's window, so the common
        // no-dictionary commit skips it.
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
                    // `borrowed_supported()` keeps the optimal parsers on the
                    // owned path and `set_borrowed_block` asserts it.
                    other => {
                        unreachable!("HashChain backend with unexpected borrowed search {other:?}")
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
            }
            SearchMethod::DoubleFast => {
                self.dfast_matcher_mut()
                    .start_matching(&mut handle_sequence);
            }
            SearchMethod::RowHash | SearchMethod::BinaryTreeLazy => {
                // One upstream `lazy_generic` body for greedy (depth 0: a
                // repcode hit is stored without a search, no lookahead), lazy
                // / lazy2 (`lazy_depth` 1 / 2) and btlazy2 (depth 2 over the
                // binary-tree finder); the finder (rows / chain / tree) was
                // resolved at `configure`.
                self.row_matcher_mut().start_matching(&mut handle_sequence);
            }
            SearchMethod::HashChain => {
                // Greedy/lazy/lazy2 all flow through the lazy parser; it
                // reads `hc.lazy_depth` (0 = greedy commit).
                self.hc_matcher_mut()
                    .start_matching_lazy(&mut handle_sequence);
            }
            SearchMethod::BinaryTree => match self.strategy_tag {
                StrategyTag::BtOpt => self.compress_block::<strategy::BtOpt>(&mut handle_sequence),
                StrategyTag::BtUltra => {
                    self.compress_block::<strategy::BtUltra>(&mut handle_sequence)
                }
                StrategyTag::BtUltra2 => {
                    self.compress_block::<strategy::BtUltra2>(&mut handle_sequence)
                }
                _ => unreachable!(
                    "SearchMethod::BinaryTree requires an optimal strategy tag (BtOpt/BtUltra/BtUltra2)"
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

#[cfg(feature = "bench-internals")]
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

#[cfg(feature = "bench-internals")]
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
#[cfg(feature = "bench-internals")]
pub(crate) fn collect_level22_sequences(data: &[u8]) -> Vec<(usize, usize, usize)> {
    merge_block_delimiters(collect_level22_sequences_with_delimiters(data))
        .into_iter()
        .filter(|(_, offset, match_len)| *offset != 0 || *match_len != 0)
        .collect()
}

#[cfg(feature = "bench-internals")]
fn collect_level22_sequences_with_delimiters(data: &[u8]) -> Vec<(usize, usize, usize)> {
    let mut driver = MatchGeneratorDriver::new(super::cost_model::HC_BLOCKSIZE_MAX, 1);
    driver.set_source_size_hint(data.len() as u64);
    driver.reset(CompressionLevel::Level(22));

    let mut sequences = Vec::new();
    for (chunk_start, chunk_len) in level22_block_ranges(data) {
        driver.commit_input(&data[chunk_start..chunk_start + chunk_len]);
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
mod tests;
