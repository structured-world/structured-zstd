//! Per-level compression tuning: the matcher config structs, the level
//! parameter table, and the level → params resolution chain.
//!
//! Moved verbatim from `match_generator.rs` (no behaviour change): the
//! `HcConfig` / `RowConfig` / `DfastConfig` / `FastConfig` knobs, `LevelParams`
//! and `LEVEL_TABLE`, the public-parameter overrides, the source-size tiering,
//! and the workspace estimators. `match_generator` imports this resolution API
//! instead of carrying it inline. Encoding-level paths are written absolute
//! (`crate::encoding::…`) so the module can live under `levels/` unchanged.

use crate::encoding::CompressionLevel;
#[cfg(test)]
use crate::encoding::match_generator::{
    HC_SEARCH_DEPTH, HC_TARGET_LEN, ROW_HASH_BITS, ROW_LOG, ROW_MIN_MATCH_LEN, ROW_SEARCH_DEPTH,
    ROW_TARGET_LEN,
};
#[cfg(test)]
use crate::encoding::match_table::storage::{HC_CHAIN_LOG, HC_HASH_LOG};
/// Bundled tuning knobs for the hash-chain matcher. Using a typed config
/// instead of positional `usize` args eliminates parameter-order hazards.
#[derive(Copy, Clone, PartialEq, Eq)]
pub(crate) struct HcConfig {
    pub(crate) hash_log: usize,
    pub(crate) chain_log: usize,
    pub(crate) search_depth: usize,
    pub(crate) target_len: usize,
    /// Key width the binary tree hashes, `BOUNDED(4, minMatch, 6)`: upstream
    /// templates its finder on `mls = BOUNDED(3, minMatch, 6)`
    /// (`ZSTD_selectBtGetAllMatches`, zstd_opt.c), but its `ZSTD_hashPtr` has no
    /// 3-byte case and hashes `mls = 3` on four bytes (the `default` arm,
    /// zstd_compress_internal.h), taking 3-byte matches from the hash3 table.
    pub(crate) search_mls: usize,
    /// Upstream `cParams.minMatch`. A 3 builds the hash3 table that supplies the
    /// 3-byte matches (`ZSTD_reset_matchState`: `hashLog3 = minMatch == 3 ?
    /// MIN(17, windowLog) : 0`).
    pub(crate) min_match: usize,
}

impl HcConfig {
    /// Whether the frame searches the hash3 table for 3-byte matches.
    pub(crate) const fn searches_hash3(&self) -> bool {
        self.min_match == 3
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub(crate) struct RowConfig {
    pub(crate) hash_bits: usize,
    pub(crate) row_log: usize,
    pub(crate) search_depth: usize,
    pub(crate) target_len: usize,
    /// Upstream zstd `cParams.minMatch` for the row matcher: the regular-search
    /// acceptance floor (a row candidate must extend to >= `mls` bytes).
    /// The C-like advanced API surfaces this as the row min-match knob.
    /// `ROW_MIN_MATCH_LEN` (5) is the default; the row hash key width stays
    /// 4 bytes (an internal detail), so this only tunes the acceptance
    /// floor, not the candidate hash distribution.
    pub(crate) mls: usize,
    /// Upstream `cParams.chainLog`: the hash-chain table the greedy/lazy
    /// parse searches instead of rows when the window is 2^14 or smaller
    /// (upstream `ZSTD_resolveRowMatchFinderMode`), or the binary tree
    /// (`2^(chainLog-1)` nodes) of a btlazy2 level.
    pub(crate) chain_log: usize,
    /// Upstream `ZSTD_btlazy2`: the lazy parse searches the lazily-sorted
    /// binary tree (`ZSTD_BtFindBestMatch`) instead of rows / the chain.
    pub(crate) bt: bool,
    /// Rows or the hash chain, when not the tree (`ZSTD_c_useRowMatchFinder`).
    pub(crate) match_finder: crate::encoding::parameters::RowMatchFinderMode,
}

impl RowConfig {
    /// Whether a frame without a dictionary searches rows with a `window`-byte
    /// window (a dictionary frame takes its dictionary's answer).
    pub(crate) const fn searches_rows(&self, window: usize) -> bool {
        !self.bt && self.match_finder.searches_rows(window)
    }
}

// Only used as the default HashChain config when the test-only parse×search
// override pairs a level with a backend its native row doesn't populate.
#[cfg(test)]
pub(crate) const HC_CONFIG: HcConfig = HcConfig {
    hash_log: HC_HASH_LOG,
    chain_log: HC_CHAIN_LOG,
    search_depth: HC_SEARCH_DEPTH,
    target_len: HC_TARGET_LEN,
    search_mls: 4,
    min_match: 4,
};

// Default Row config: only used by tests and the test-only parse×search
// override (production greedy L5 carries its own `ROW_L5`).
#[cfg(test)]
pub(crate) const ROW_CONFIG: RowConfig = RowConfig {
    hash_bits: ROW_HASH_BITS,
    row_log: ROW_LOG,
    search_depth: ROW_SEARCH_DEPTH,
    target_len: ROW_TARGET_LEN,
    mls: ROW_MIN_MATCH_LEN,
    chain_log: ROW_HASH_BITS,
    bt: false,
    match_finder: crate::encoding::parameters::RowMatchFinderMode::Auto,
};

/// Per-level Double-Fast hash sizing, mirroring the upstream zstd `clevels.h` columns
/// (config-driven, not a hardcoded constant): `long_hash_log` =
/// `cParams.hashLog` (the long 8-byte hash table), `short_hash_log` =
/// `cParams.chainLog` (the short hash table dfast repurposes as its
/// secondary index), `min_match` = `cParams.minMatch` (the short table's key
/// width). Only the Dfast backend reads it, so non-dfast level rows carry
/// `dfast: None`.
#[derive(Copy, Clone, PartialEq, Eq)]
pub(crate) struct DfastConfig {
    pub(crate) long_hash_log: u8,
    pub(crate) short_hash_log: u8,
    pub(crate) min_match: u8,
}

// Upstream zstd clevels.h default row (srcSize > 256 KB): L3 {hashLog 17,
// chainLog 16, minMatch 5}. Only the test-only parse×search override reads it.
#[cfg(test)]
pub(crate) const DFAST_L3: DfastConfig = DfastConfig {
    long_hash_log: 17,
    short_hash_log: 16,
    min_match: 5,
};

/// Per-level Fast-strategy tuning, only consumed by the `FastKernelMatcher`
/// (Simple backend): `hash_log` = upstream zstd `cParams.hashLog`, `mls` = upstream zstd
/// `cParams.minMatch` (4..=8), `step_size` = upstream zstd `stepSize`. Carried as
/// `LevelParams.fast` (`Some` only on Fast level rows; `None` elsewhere).
#[derive(Copy, Clone, PartialEq, Eq)]
pub(crate) struct FastConfig {
    pub(crate) hash_log: u32,
    pub(crate) mls: u32,
    pub(crate) step_size: usize,
}

// The level-1 row at the >256 KiB tier. Only the test-only parse×search
// override reads it.
#[cfg(test)]
pub(crate) const FAST_L1: FastConfig = FastConfig {
    hash_log: 14,
    mls: 7,
    step_size: 2,
};

/// Resolved tuning parameters for a compression level and source size. The
/// [`StrategyTag`] selects the compile-time strategy consts; the runtime
/// [`BackendTag`] used by the driver dispatcher comes from the resolved
/// search method ([`Self::backend`]), so an override or a size tier that
/// changes the search changes the backend with it.
///
/// [`StrategyTag`]: crate::encoding::strategy::StrategyTag
/// [`BackendTag`]: crate::encoding::strategy::BackendTag
#[derive(Copy, Clone, PartialEq, Eq)]
pub(crate) struct LevelParams {
    pub(crate) strategy_tag: crate::encoding::strategy::StrategyTag,
    /// Decoupled search-method axis. Independent of `strategy_tag`'s
    /// parse half: a level can pair any parse (greedy / lazy depth via
    /// `lazy_depth`) with any search backend here. Defaults to the
    /// historical pairing (`strategy_tag.search()`) but is overridable
    /// per level so the parse×search matrix can be swept and tuned.
    pub(crate) search: crate::encoding::strategy::SearchMethod,
    pub(crate) window_log: u8,
    pub(crate) lazy_depth: u8,
    /// Per-strategy tuning. Exactly one is `Some` on each level row, matching
    /// `strategy_tag`'s backend, so the table self-documents which knobs a
    /// level actually consumes (the others are `None`, not dead placeholders):
    /// `fast` for the Fast/Simple backend, `dfast` for Double-Fast, `hc` for
    /// the HashChain (lazy / btopt / btultra*) backend, `row` for the Row
    /// (greedy L5) backend.
    pub(crate) fast: Option<FastConfig>,
    pub(crate) dfast: Option<DfastConfig>,
    pub(crate) hc: Option<HcConfig>,
    pub(crate) row: Option<RowConfig>,
}

impl LevelParams {
    /// Backend family (storage variant) for the driver dispatcher.
    /// Derived from the decoupled `search` axis so a level can route to
    /// a different search backend than its `strategy_tag` historically
    /// implied.
    pub(crate) fn backend(&self) -> crate::encoding::strategy::BackendTag {
        self.search.backend()
    }

    /// Parse mode derived from the decoupled `search` axis: the binary-tree
    /// search path carries `ParseMode::Optimal`; every other search backend
    /// derives greedy/lazy/lazy2 from `lazy_depth`. Reading `search` (not the
    /// strategy tag) keeps the parse×search decoupling complete even when a
    /// level whose tag is `Bt*` is overridden to a non-BT search backend.
    pub(crate) fn parse(&self) -> crate::encoding::strategy::ParseMode {
        match self.search {
            crate::encoding::strategy::SearchMethod::BinaryTree => {
                crate::encoding::strategy::ParseMode::Optimal
            }
            _ => crate::encoding::strategy::ParseMode::from_lazy_depth(self.lazy_depth),
        }
    }

    /// Cheap fingerprint pre-splitter level (the `ZSTD_splitBlock` level;
    /// `0` = from-borders heuristic, `1..=4` = byChunks with sampling tier
    /// `level - 1`, rates 43 / 11 / 5 / 1). See [`pre_split_for`].
    pub(crate) fn pre_split(&self) -> Option<u8> {
        Some(pre_split_for(self.strategy_tag, self.lazy_depth))
    }

    /// `log2` of how much of a dictionary's end the match-finder tables index
    /// when it is loaded: a longer dictionary is indexed only that far from its
    /// end, since tables this size could not keep more of it anyway (upstream
    /// `ZSTD_loadDictionaryContent`, `zstd_compress.c:4987`:
    /// `MIN(MAX(hashLog + 3, chainLog + 1), 31)`). Every byte of it stays in
    /// the window.
    pub(crate) fn dictionary_reach_log(&self) -> u32 {
        use crate::encoding::strategy::SearchMethod;
        // The active backend's config: an override may leave the level row's
        // own beside it.
        let logs = match self.search {
            SearchMethod::Fast => self.fast.map(|fast| (fast.hash_log as usize, 0)),
            SearchMethod::DoubleFast => self
                .dfast
                .map(|dfast| (dfast.long_hash_log as usize, dfast.short_hash_log as usize)),
            SearchMethod::RowHash | SearchMethod::BinaryTreeLazy => {
                self.row.map(|row| (row.hash_bits, row.chain_log))
            }
            SearchMethod::BinaryTree => self.hc.map(|hc| (hc.hash_log, hc.chain_log)),
        };
        // The reset synthesises the active config when a row lacks it, so a
        // missing one only means a table-less level: nothing to index.
        let Some((hash_log, chain_log)) = logs else {
            return 0;
        };
        (hash_log + 3).max(chain_log + 1).min(31) as u32
    }

    /// The level's upstream `cParams.minMatch`: the shortest match its search
    /// accepts, and so the shortest piece of a long-distance match a block cut
    /// leaves worth keeping (`maybeSplitSequence`, `zstd_ldm.c:644`).
    #[cfg(feature = "ldm")]
    pub(crate) fn min_match(&self) -> usize {
        use crate::encoding::strategy::SearchMethod;
        let configured = match self.search {
            SearchMethod::Fast => self.fast.map(|fast| fast.mls as usize),
            SearchMethod::DoubleFast => Some(crate::encoding::match_generator::DFAST_MIN_MATCH_LEN),
            SearchMethod::RowHash | SearchMethod::BinaryTreeLazy => self.row.map(|row| row.mls),
            SearchMethod::BinaryTree => self.hc.map(|hc| hc.search_mls),
        };
        configured.unwrap_or(crate::encoding::cost_model::HC_FORMAT_MINMATCH)
    }
}

/// The pre-splitter level for an effective strategy (the tag plus, for the
/// collapsed `Lazy` tag, its lazy depth).
///
/// Upstream's default is `splitLevels[strategy] = {0,0,1,2,2,3,3,4,4,4}`
/// (zstd_compress.c:4552, used as is; only an explicit `blockSplitterLevel`
/// is shifted down by 2). It is followed up to lazy depth 1 — the finer
/// sampling is a real ratio win on mixed data (decodecorpus at greedy/lazy:
/// the borders tier compressed 4.6-4.9 % WORSE than upstream, the upstream
/// rate-11 tier <= upstream) — but the lazy2/btlazy2 rate-5 and optimal-band
/// rate-1 tiers are DELIBERATELY kept two steps coarser: on periodic input
/// they over-split every block into tiny pieces exactly like upstream does
/// (100 MiB of repeated log lines at L8-L12: 140,625 bytes and 3.6x the
/// time upstream-tier, 9,742 bytes coarse — 14x better than upstream) while
/// on real data they buy under 0.1 % (decodecorpus L8-L12: 170 bytes of
/// 483 KiB). The drop-in contract asks for ratio <= upstream, not for
/// upstream's block boundaries.
pub(crate) fn pre_split_for(tag: crate::encoding::strategy::StrategyTag, lazy_depth: u8) -> u8 {
    use crate::encoding::strategy::StrategyTag;
    match tag {
        // Upstream tiers: borders / byChunks rate 43 / rate 11.
        StrategyTag::Fast => 0,
        StrategyTag::Dfast => 1,
        StrategyTag::Greedy => 2,
        // lazy (depth 1) = upstream rate 11; lazy2 coarsened to rate 43.
        StrategyTag::Lazy => {
            if lazy_depth >= 2 {
                1
            } else {
                2
            }
        }
        StrategyTag::Btlazy2 => 1,
        // Coarsened to byChunks rate 11 (upstream: rate 1).
        StrategyTag::BtOpt | StrategyTag::BtUltra | StrategyTag::BtUltra2 => 2,
    }
}

/// [`resolve_level_params`] under the caller's knobs, for a frame without a
/// dictionary: upstream `ZSTD_getCParamsFromCCtxParams`, the level's row for
/// the source, long-distance matching's window, the knobs laid over it and the
/// adjustment that bounds them, then the translation every other frame takes.
/// One path, so a knob sizes the tables exactly as the same value in a level
/// row would.
pub(crate) fn resolve_frame_params(
    level: CompressionLevel,
    source_size: Option<u64>,
    overrides: &crate::encoding::parameters::ParamOverrides,
) -> LevelParams {
    let src = source_size.unwrap_or(crate::encoding::cparams::CONTENTSIZE_UNKNOWN);
    if overrides.is_empty() {
        return resolve_level_params(level, source_size);
    }
    if matches!(level, CompressionLevel::Uncompressed) {
        // A raw frame searches nothing, so only the window it declares answers
        // to the knobs, bounded by the source like every other window.
        let mut params = resolve_level_params(level, source_size);
        if let Some(window_log) = overrides.window_log {
            params.window_log =
                (crate::encoding::cparams::adjusted_window_log(u32::from(window_log), src, 0)
                    as u8)
                    .max(MIN_WINDOW_LOG);
        }
        return params;
    }
    let cp = crate::encoding::cparams::get_frame_cparams(numeric_level(level), src, 0, overrides);
    let mut params = level_params_from_cparams(cp);
    if let Some(row) = params.row.as_mut() {
        row.match_finder = overrides.row_match_finder;
    }
    params
}

/// The key width the fast strategy hashes for a `minMatch`: upstream's fast
/// block compressor takes 3 as 4 (zstd_fast.c, `ZSTD_compressBlock_fast`:
/// `default: /* includes case 3 */`). A 3 reaches the fast strategy from the
/// knob, or from an optimal level's CDict row a strategy knob moved onto it.
fn fast_key_len(min_match: u32) -> u32 {
    min_match.max(4)
}

/// Map the resolved runtime strategy to the upstream zstd LDM strategy ordinal
/// (1..=9) that [`crate::encoding::ldm::params::LdmParams::derive`] expects.
/// The collapsed `Lazy` tag splits on `lazy_depth` (lazy = 4, lazy2 = 5).
#[cfg(feature = "ldm")]
pub(crate) fn ldm_strategy_ordinal(
    tag: crate::encoding::strategy::StrategyTag,
    lazy_depth: u8,
) -> u32 {
    use crate::encoding::strategy::StrategyTag;
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
        // Upstream zstd `ZSTD_btlazy2` ordinal.
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
/// primed-dictionary snapshot — see `match_generator::PrimedKey`. Operates on the full `u64`
/// so callers comparing a hint against a cutoff get the same bucketed decision
/// here and at the driver, with no `as usize` truncation on 32-bit targets.
pub(crate) fn source_size_ceil_log(size: u64) -> u8 {
    if size == 0 {
        MIN_WINDOW_LOG
    } else {
        (64 - (size - 1).leading_zeros()) as u8
    }
}

/// Largest dictionary region (bytes) the Fast attach path can index. The tagged
/// dict table packs each position into `32 - DICT_TAG_BITS` (= 24) bits, so a
/// region past `2^24` (16 MiB) would overflow the packed position. Dictionaries
/// this large fall back to COPY mode, whose live table stores full `u32`
/// positions and handles them. The size hint set on dict load equals the actual
/// dict content length, so the attach-vs-copy decision (and the matching
/// snapshot-key / epoch bits) can gate on it consistently at reset time.
pub(crate) const MAX_FAST_ATTACH_DICT_REGION: usize = 1 << 24;

/// Smallest window_log the encoder will use regardless of source size.
pub(crate) const MIN_WINDOW_LOG: u8 = 10;

/// Largest window_log the public parameter API accepts
/// ([`CParameter::WindowLog`](crate::encoding::CParameter)'s upper bound), and
/// so the largest a workspace estimate can be asked to describe. The estimate
/// answers for whatever it is given, and shifting by more than the width of a
/// `usize` is undefined, so a request beyond this is answered with this.
pub(crate) const MAX_ESTIMATED_WINDOW_LOG: u8 = 30;

/// Translate a verbatim upstream `ZSTD_defaultCParameters[tier][level]` row
/// (`cparams::CParams`) into our resolved [`LevelParams`], reproducing
/// upstream's cParams -> matcher-config derivation so the encoder follows C's
/// source-size-tiered STRATEGY + table widths rather than a single hand-tuned
/// `LEVEL_TABLE`. Derivation (verified against L6/L16/L22 vs clevels.h):
/// `search_depth = 1 << searchLog`; row `row_log = clamp(searchLog, 4, 6)`; the
/// per-strategy sub-config carries the verbatim `hashLog` / `chainLog` /
/// `targetLength` / `minMatch`. Strategy numbers are upstream `ZSTD_strategy`
/// (fast=1, dfast=2, greedy=3, lazy=4, lazy2=5, btlazy2=6, btopt=7, btultra=8,
/// btultra2=9).
fn level_params_from_cparams(cp: crate::encoding::cparams::CParams) -> LevelParams {
    use crate::encoding::strategy::{SearchMethod, StrategyTag};
    let window_log = cp.window_log as u8;
    let search_depth = 1usize << cp.search_log;
    let target_len = cp.target_length as usize;
    let hc = HcConfig {
        hash_log: cp.hash_log as usize,
        chain_log: cp.chain_log as usize,
        search_depth,
        target_len,
        // See `HcConfig::search_mls`: a `minMatch` of 3 hashes four bytes.
        search_mls: cp.min_match.clamp(4, 6) as usize,
        min_match: cp.min_match as usize,
    };
    let row = RowConfig {
        hash_bits: cp.hash_log as usize,
        row_log: cp.search_log.clamp(4, 6) as usize,
        search_depth,
        target_len,
        mls: cp.min_match as usize,
        chain_log: cp.chain_log as usize,
        // Upstream `ZSTD_btlazy2` (strategy 6).
        bt: cp.strategy == 6,
        match_finder: crate::encoding::parameters::RowMatchFinderMode::Auto,
    };
    let bt = |tag| LevelParams {
        strategy_tag: tag,
        search: SearchMethod::BinaryTree,
        window_log,
        lazy_depth: 2,
        fast: None,
        dfast: None,
        hc: Some(hc),
        row: None,
    };
    let row_lvl = |tag, search, lazy_depth| LevelParams {
        strategy_tag: tag,
        search,
        window_log,
        lazy_depth,
        fast: None,
        dfast: None,
        hc: None,
        row: Some(row),
    };
    match cp.strategy {
        1 => LevelParams {
            strategy_tag: StrategyTag::Fast,
            search: SearchMethod::Fast,
            window_log,
            lazy_depth: 0,
            // Upstream fast `stepSize`: `targetLength + 1` (0 -> 1, so step 2).
            fast: Some(FastConfig {
                hash_log: cp.hash_log,
                mls: fast_key_len(cp.min_match),
                step_size: target_len.max(1) + 1,
            }),
            dfast: None,
            hc: None,
            row: None,
        },
        2 => LevelParams {
            strategy_tag: StrategyTag::Dfast,
            search: SearchMethod::DoubleFast,
            window_log,
            lazy_depth: 1,
            fast: None,
            dfast: Some(DfastConfig {
                long_hash_log: cp.hash_log as u8,
                short_hash_log: cp.chain_log as u8,
                min_match: cp.min_match as u8,
            }),
            hc: None,
            row: None,
        },
        3 => row_lvl(StrategyTag::Greedy, SearchMethod::RowHash, 0),
        4 => row_lvl(StrategyTag::Lazy, SearchMethod::RowHash, 1),
        5 => row_lvl(StrategyTag::Lazy, SearchMethod::RowHash, 2),
        // btlazy2: the same lazy2 parse over the lazily-sorted binary tree.
        6 => row_lvl(StrategyTag::Btlazy2, SearchMethod::BinaryTreeLazy, 2),
        7 => bt(StrategyTag::BtOpt),
        8 => bt(StrategyTag::BtUltra),
        _ => bt(StrategyTag::BtUltra2),
    }
}

/// The long-distance matcher's parameters for a frame: the caller-pinned knobs
/// seeded first, then the upstream derivation fills the rest so the set stays
/// consistent (`hash_rate_log = window_log - hash_log`, and so on); clobbering
/// after the derivation would hand the producer an inconsistent set. Shared by
/// the matcher's reset and the workspace estimate.
#[cfg(feature = "ldm")]
pub(crate) fn frame_ldm_params(
    params: &LevelParams,
    ldm: &crate::encoding::parameters::LdmOverride,
) -> crate::encoding::ldm::params::LdmParams {
    let seed = crate::encoding::ldm::params::LdmParams {
        window_log: params.window_log as u32,
        hash_log: ldm.hash_log.unwrap_or(0),
        hash_rate_log: ldm.hash_rate_log.unwrap_or(0),
        min_match_length: ldm.min_match.unwrap_or(0),
        bucket_size_log: ldm.bucket_size_log.unwrap_or(0),
    };
    seed.derive(ldm_strategy_ordinal(params.strategy_tag, params.lazy_depth))
}

/// Estimated steady-state heap footprint of a one-shot compression context
/// at `level` (window history + match-finder tables + block staging), in
/// bytes. Computed from the same per-level tuning table the encoder
/// resolves at frame start, so the estimate tracks the real allocations;
/// it is an upper-bound style budget figure, not an exact accounting.
pub fn estimated_compression_workspace_bytes(level: CompressionLevel) -> usize {
    estimated_compression_workspace_bytes_for_source(level, None)
}

/// The same estimate for a source whose size is known.
///
/// The window and the match-finder tables are capped by the source, so a level
/// that would reserve hundreds of MiB for an arbitrary stream reserves a
/// fraction of that for a small one — and a caller budgeting memory for a
/// compression it is about to run knows which. `None` is the arbitrary-stream
/// figure that [`estimated_compression_workspace_bytes`] reports.
pub fn estimated_compression_workspace_bytes_for_source(
    level: CompressionLevel,
    src_size_hint: Option<u64>,
) -> usize {
    estimated_compression_workspace_bytes_for_run(level, src_size_hint, None, false, None)
}

/// The same estimate for a run whose window, long-distance matching and
/// dictionary are what the caller has actually asked for.
///
/// A `window_log` override enlarges the history the frame keeps, and
/// long-distance matching adds a hash table of its own on top — neither of them
/// visible in the level's own preset. A dictionary goes further than adding to
/// the figure: it *decides* it. The frame runs the dictionary's own compression
/// parameters, so a small source compressed against a large dictionary builds
/// tables sized for the dictionary, which at the higher levels is the
/// difference between tens of KiB and hundreds of MiB. `None`, `false` and
/// `None` give the preset, which is what
/// [`estimated_compression_workspace_bytes_for_source`] reports.
///
/// `dictionary` is what a caller weighing a run before it parses the blob can
/// answer with [`DictionarySizes::raw_content`] on the blob's own length: the
/// content of a trained dictionary is smaller than the blob it came in, and
/// overstating it can only move the estimate toward the copy-mode geometry,
/// which is the larger of the two.
///
/// [`DictionarySizes::raw_content`]: crate::encoding::DictionarySizes::raw_content
pub fn estimated_compression_workspace_bytes_for_run(
    level: CompressionLevel,
    src_size_hint: Option<u64>,
    window_log: Option<u8>,
    long_distance_matching: bool,
    dictionary: Option<crate::encoding::DictionarySizes>,
) -> usize {
    // The knobs go through the encoder's own resolution, so the window is
    // bounded the same two ways: by the format's floor and by what the source
    // can fill. A request past the widest window the encoder accepts is
    // answered with that window: shifting by more than the type's width is
    // undefined rather than merely large.
    let overrides = crate::encoding::parameters::ParamOverrides {
        window_log: window_log.map(|requested| requested.min(MAX_ESTIMATED_WINDOW_LOG)),
        ldm: long_distance_matching.then(crate::encoding::parameters::LdmOverride::default),
        ..Default::default()
    };
    let params = match dictionary.filter(|sizes| sizes.content != 0) {
        Some(sizes) => {
            resolve_level_params_with_dict(level, src_size_hint, sizes, &overrides).params
        }
        None => resolve_frame_params(level, src_size_hint, &overrides),
    };
    // The long-distance matcher's own table, sized from the window it searches
    // (upstream `ZSTD_ldm_adjustParameters`). Only the `ldm` build has one.
    #[cfg(feature = "ldm")]
    let ldm = overrides.ldm.map_or(0, |ldm| {
        crate::encoding::ldm::estimated_workspace_bytes(&frame_ldm_params(&params, &ldm))
    });
    #[cfg(not(feature = "ldm"))]
    let ldm = 0;
    workspace_bytes(&params, ldm)
}

/// The level params a frame resolves to under `parameters`, for a source of
/// `src_size_hint` bytes and an optional dictionary: the level (or the
/// dictionary's tier), then the caller's overrides, as the matcher's reset
/// resolves them.
pub(crate) fn frame_level_params(
    parameters: &crate::encoding::CompressionParameters,
    src_size_hint: Option<u64>,
    dictionary: Option<crate::encoding::DictionarySizes>,
) -> LevelParams {
    let level = parameters.level();
    let overrides = parameters.overrides();
    match dictionary.filter(|sizes| sizes.content != 0) {
        Some(sizes) => {
            resolve_level_params_with_dict(level, src_size_hint, sizes, &overrides).params
        }
        None => resolve_frame_params(level, src_size_hint, &overrides),
    }
}

/// The workspace estimate for a frame run under `parameters`: the level, every
/// knob that overrides it, the source size and the dictionary, resolved the way
/// the encoder resolves them at frame start.
///
/// [`estimated_compression_workspace_bytes_for_run`] answers for a level with a
/// window and long-distance matching on top; a caller that also sets `hashLog`,
/// `chainLog`, a strategy or the long-distance matcher's own table sizes needs
/// this one, since each of those resizes what the frame allocates. A dictionary
/// frame runs the geometry the dictionary is prepared with, the knobs included,
/// as the encoder does.
///
/// # Examples
///
/// ```
/// use structured_zstd::encoding::{
///     estimated_compression_workspace_bytes_for_parameters, CompressionLevel,
///     CompressionParameters,
/// };
///
/// let level = CompressionParameters::builder(CompressionLevel::Level(3)).build().unwrap();
/// let wide = CompressionParameters::builder(CompressionLevel::Level(3))
///     .window_log(27)
///     .hash_log(24)
///     .build()
///     .unwrap();
/// let source = Some(512 << 20);
/// assert!(
///     estimated_compression_workspace_bytes_for_parameters(&wide, source, None)
///         > estimated_compression_workspace_bytes_for_parameters(&level, source, None)
/// );
/// ```
pub fn estimated_compression_workspace_bytes_for_parameters(
    parameters: &crate::encoding::CompressionParameters,
    src_size_hint: Option<u64>,
    dictionary: Option<crate::encoding::DictionarySizes>,
) -> usize {
    let params = frame_level_params(parameters, src_size_hint, dictionary);
    #[cfg(feature = "ldm")]
    let ldm = parameters.overrides().ldm.map_or(0, |ldm| {
        crate::encoding::ldm::estimated_workspace_bytes(&frame_ldm_params(&params, &ldm))
    });
    #[cfg(not(feature = "ldm"))]
    let ldm = 0;
    workspace_bytes(&params, ldm)
}

/// `entry` bytes for each of `1 << log` slots, pinned at `usize::MAX` where
/// the table is more than a `usize` counts: the estimate is a budget, and a
/// figure that wrapped would call an impossible table affordable.
fn table_bytes(entry: usize, log: usize) -> usize {
    u32::try_from(log)
        .ok()
        .and_then(|log| 1usize.checked_shl(log))
        .and_then(|slots| slots.checked_mul(entry))
        .unwrap_or(usize::MAX)
}

/// Everything a compressor keeps for a frame resolved to `params`, plus `ldm`
/// bytes of long-distance table: the workspace (history, match-finder tables
/// and block buffers, laid out by the same functions the reset uses), and
/// what lives beside it (optimal-parser scratch, entropy tables, the block
/// splitter's state, the window's block deque).
fn workspace_bytes(params: &LevelParams, ldm: usize) -> usize {
    use crate::encoding::strategy::{SearchMethod, StrategyTag};
    use crate::encoding::workspace::{ALIGN, region_bytes};
    // A 30-bit window is a gibibyte, which a 32-bit `usize` cannot count: the
    // widest window is more memory than such a machine has, so the figure is
    // pinned rather than wrapped.
    let window = 1usize
        .checked_shl(u32::from(params.window_log))
        .unwrap_or(usize::MAX);
    let block = (crate::common::MAX_BLOCK_SIZE as usize).min(window);
    let backend = params.backend();
    let history = crate::encoding::match_generator::history_ceiling(backend, window, 0, block);
    // Mirror `configure()`: the HC3 short-match side table exists only for a
    // `minMatch` of 3, capped by the window log; the BT pointer-pair layout
    // fits inside the `4 << chain_log` chain term (pairs over `chain_log - 1`
    // nodes).
    let wants_hash3 = params.hc.is_some_and(|hc| hc.searches_hash3());
    let uses_bt = matches!(
        params.strategy_tag,
        StrategyTag::BtOpt | StrategyTag::BtUltra | StrategyTag::BtUltra2
    );
    // Only the backend `params.search` selects is built: `reset` swaps in one
    // matcher storage per frame. A strategy override leaves the level's own
    // row in place beside the one it synthesized, so summing every populated
    // config would charge a frame for tables it never allocates.
    // Every term goes through `table_bytes` and every sum saturates: an
    // override can ask for a table past what a 32-bit `usize` counts, and a
    // shift that dropped its high bits would report that table as free.
    let tables = match params.search {
        SearchMethod::Fast => params
            .fast
            .map_or(0, |f| table_bytes(4, f.hash_log as usize)),
        SearchMethod::DoubleFast => params.dfast.map_or(0, |d| {
            table_bytes(4, usize::from(d.long_hash_log))
                .saturating_add(table_bytes(4, usize::from(d.short_hash_log)))
        }),
        // The lazy backend's chain / tree finders use a plain hash table
        // (`4 << hash_bits`) plus the chain / tree table (`4 << chain_log`)
        // instead of the row tables.
        SearchMethod::RowHash | SearchMethod::BinaryTreeLazy => params.row.map_or(0, |r| {
            if !r.searches_rows(window) {
                table_bytes(4, r.hash_bits).saturating_add(table_bytes(4, r.chain_log))
            } else {
                table_bytes(4, r.hash_bits).saturating_add(table_bytes(2, r.hash_bits))
            }
        }),
        SearchMethod::BinaryTree => params.hc.map_or(0, |h| {
            let hash3 = if wants_hash3 {
                table_bytes(
                    4,
                    crate::encoding::match_table::storage::HC3_HASH_LOG
                        .min(params.window_log as usize),
                )
            } else {
                0
            };
            table_bytes(4, h.hash_log)
                .saturating_add(table_bytes(4, h.chain_log))
                .saturating_add(hash3)
        }),
    };
    // BT modes box a `BtMatcher`; its retained scratch layout is budgeted
    // next to the struct so estimator and allocator evolve together.
    let bt = if uses_bt {
        crate::encoding::bt::BtMatcher::estimated_workspace_bytes()
    } else {
        0
    };
    // One allocation: each table region and the history rounded to `ALIGN`,
    // the block buffers behind them, and the padding that aligns its start.
    let laid_out = tables
        .saturating_add(3 * ALIGN)
        .saturating_add(region_bytes::<u8>(history))
        .saturating_add(crate::encoding::blocks::CompressedBlockScratch::workspace_bytes(block))
        .saturating_add(ALIGN - 1);
    // The backends that slide their window by blocks keep one length per
    // block in it, in a deque that grows by doubling. Plain arithmetic here
    // and in `beside`: `history / block` is at most the window over the
    // smallest block plus a few, and every term of `beside` is a constant or
    // bounded by the 128 KiB block, so neither comes near `usize::MAX`.
    let block_deque = if matches!(backend, crate::encoding::strategy::BackendTag::Simple) {
        0
    } else {
        2 * (history / block + 2) * core::mem::size_of::<usize>()
    };
    let splits =
        crate::encoding::levels::fastest::post_split_enabled(params.strategy_tag, window as u64);
    let beside = crate::encoding::frame_compressor::FseTables::max_heap_size()
        + crate::encoding::blocks::CompressedBlockScratch::max_retained_heap_size(block, splits)
        + 2 * crate::huff0::huff0_encoder::HuffmanTable::max_heap_size()
        + crate::huff0::huff0_encoder::WeightScratch::max_heap_size();
    // Saturating: the parts are each bounded, but their sum at the widest
    // window is more than a 32-bit `usize` counts, and a total that wrapped
    // would report a run as fitting a limit it cannot.
    laid_out
        .saturating_add(bt)
        .saturating_add(block_deque)
        .saturating_add(beside)
        .saturating_add(ldm)
}

/// Extra steady-state workspace the optimal strategies (ordinals 7..=9,
/// btopt..btultra2) retain beyond the hash/chain tables: the boxed matcher
/// plus its scratch arenas, and for a `min_match` of 3 the short-match side
/// table those matches come from (capped by the window log, as upstream sizes
/// `hashLog3`). 0 for the other ordinals (btlazy2's tree lives in the lazy
/// backend's chain table).
///
/// # Examples
///
/// ```
/// use structured_zstd::encoding::estimated_bt_strategy_extra_bytes;
///
/// // btopt keying on four bytes has no short-match table; on three it does.
/// let four = estimated_bt_strategy_extra_bytes(7, 20, 4);
/// let three = estimated_bt_strategy_extra_bytes(7, 20, 3);
/// assert!(three > four);
/// assert_eq!(estimated_bt_strategy_extra_bytes(3, 20, 3), 0);
/// ```
pub fn estimated_bt_strategy_extra_bytes(
    strategy_ordinal: u32,
    window_log: u32,
    min_match: u32,
) -> usize {
    if !(7..=9).contains(&strategy_ordinal) {
        return 0;
    }
    let hash3 = if min_match == 3 {
        4usize << crate::encoding::match_table::storage::HC3_HASH_LOG.min(window_log as usize)
    } else {
        0
    };
    crate::encoding::bt::BtMatcher::estimated_workspace_bytes() + hash3
}

/// Resolve a [`CompressionLevel`] (+ optional source-size hint) to the
/// concrete [`LevelParams`] the matcher runs: strategy tag, search method
/// (match-finder), window log, and per-backend config.
///
/// ## CRITICAL: input size changes the match-finder (and can change strategy)
///
/// The resolved geometry is a function of the SOURCE SIZE, not the level
/// alone. This is the easy-to-miss part (so read this before assuming a level
/// maps to one fixed match-finder). Every level except `Uncompressed` goes
/// through [`get_cparams`](crate::encoding::cparams::get_cparams), the port of
/// upstream zstd `ZSTD_getCParams_internal`, which runs its two stages:
///
/// 1. the `clevels.h` row for the (level, source-size tier) pair. The tier
///    changes the STRATEGY as well as the table widths: level 11 is lazy2 on
///    a large source and btopt on one of 16 KiB or less;
/// 2. `ZSTD_adjustCParams_internal`, which caps `window_log` to
///    ~`ceil_log2(source_size)` and shrinks the hash and chain logs to match.
///
/// THEN, inside the Row backend, the greedy/lazy band searches a hash chain
/// instead of rows when the resolved `window_log <= 14`
/// (`RowMatchGenerator::finder`) — exactly upstream's
/// `ZSTD_resolveRowMatchFinderMode` (the Row match-finder is used for
/// greedy/lazy/lazy2 ONLY when `windowLog > 14`) — and a btlazy2 level
/// searches the lazily-sorted binary tree; the parse itself is the same
/// `lazy_generic` body in all three cases. Net effect for the SAME level:
///
/// * small input (e.g. a 10 KiB fixture → `window_log` 14) → hash chain
///   (`ZSTD_HcFindBestMatch`, scalar chain walk);
/// * large input (e.g. 1 MiB → `window_log` 20) → rows (the SIMD-tag
///   row match-finder).
///
/// A dictionary frame resolves through [`resolve_level_params_with_dict`]
/// instead: the CDict's cParams (its own size tier) with the frame's
/// source-derived `window_log`. When comparing against C on a fixture,
/// resolve the match-finder from the fixture's size (and dictionary) first,
/// or you may optimise/benchmark a path C does not even take for that input.
pub(crate) fn resolve_level_params(
    level: CompressionLevel,
    source_size: Option<u64>,
) -> LevelParams {
    // Uncompressed = raw blocks, no match-finder. Not a cParams level, so it is
    // the one row resolved by hand rather than through `get_cparams`.
    if matches!(level, CompressionLevel::Uncompressed) {
        return LevelParams {
            strategy_tag: crate::encoding::strategy::StrategyTag::Fast,
            search: crate::encoding::strategy::SearchMethod::Fast,
            // Raw frames emit literal blocks and never reference history;
            // advertising a wider window only inflates the decoder-side buffer
            // reservation, so clamp to 17 (128 KiB) regardless of input size.
            window_log: 17,
            lazy_depth: 0,
            // Beyond-upstream: hash_log=14 (vs upstream's row-0 13) for ~2× fewer
            // collisions on structured corpora; mls=6 / step_size=2 mirror the
            // upstream "base for negative" row (targetLength=1 -> step 2).
            fast: Some(FastConfig {
                hash_log: 14,
                mls: 6,
                step_size: 2,
            }),
            dfast: None,
            hc: None,
            row: None,
        };
    }
    // Every other level resolves through the SINGLE C-faithful cParams source,
    // `cparams::get_cparams` (the port of `ZSTD_getCParams`). One place selects
    // strategy + table widths + the negative-level acceleration per
    // (level, srcSize) + the source-size window/hash down-clamp, so the encoder
    // never re-derives parameters from a parallel hand-tuned path. Named presets
    // map to their numeric level; the cParams source clamps out-of-range levels
    // (>22 to 22, negatives to MIN_CLEVEL) itself.
    let numeric = numeric_level(level);
    let src = source_size.unwrap_or(crate::encoding::cparams::CONTENTSIZE_UNKNOWN);
    level_params_from_cparams(crate::encoding::cparams::get_cparams(numeric, src, 0))
}

/// The upstream numeric level a preset maps to.
pub(crate) fn numeric_level(level: CompressionLevel) -> i32 {
    match level {
        CompressionLevel::Uncompressed => unreachable!("raw frames resolve no cParams"),
        // Fastest = upstream level 1 (fast strategy, smallest real-compression
        // tables).
        CompressionLevel::Fastest => 1,
        // Default = upstream level 3 (the libzstd default).
        CompressionLevel::Default => CompressionLevel::DEFAULT_LEVEL,
        // Better = level 7: the lazy2 band — clearly above the fast/dfast levels
        // on ratio while still well under the binary-tree cost cliff.
        CompressionLevel::Better => 7,
        // Best = level 13: the first point of the deep binary-tree band that
        // strictly dominates every level below it on ratio (lower levels can tie
        // on window-bound corpora), so the alias sits on a config that always
        // wins rather than on a hair-thin margin.
        CompressionLevel::Best => 13,
        CompressionLevel::Level(n) => n,
    }
}

/// Whether a frame over `source_size` bytes (`None` = unknown) loads the
/// dictionary into tables resolved for its own size instead of running the
/// dictionary's: always when the caller asks for it, otherwise only a
/// dictionary prepared for a level of its own, once the source outgrows it
/// (upstream `ZSTD_compressBegin_internal`, zstd_compress.c, where
/// `compressionLevel == 0` keeps the CDict's and `ZSTD_dictForceLoad` loads).
fn dictionary_loads_into_frame(
    sizes: crate::encoding::DictionarySizes,
    source_size: Option<u64>,
    preference: crate::encoding::parameters::DictionaryAttachMode,
) -> bool {
    preference == crate::encoding::parameters::DictionaryAttachMode::Load
        || (sizes.prepared_at_level
            && !crate::encoding::dictionary_describes_frame(sizes.content, source_size))
}

/// How a frame's dictionary enters its match finder, decided once per frame
/// in [`resolve_level_params_with_dict`] and followed by every backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DictUse {
    /// Searched in place in tables of its own, beside the frame's
    /// (`ZSTD_resetCCtx_byAttachingCDict`).
    Attach,
    /// Merged into the frame's tables, which take the dictionary's geometry
    /// (`ZSTD_resetCCtx_byCopyingCDict`).
    Copy,
    /// Indexed into tables resolved for the frame's own source
    /// (`ZSTD_resetCCtx_internal` + `ZSTD_compress_insertDictionary`).
    Load,
}

impl DictUse {
    /// Whether the dictionary is searched in its own tables.
    pub(crate) fn attaches(self) -> bool {
        self == Self::Attach
    }
}

/// Whether a dictionary frame takes the matcher state captured after its
/// dictionary's first prime instead of priming it again: a frame of known size
/// whose tables hold the dictionary, the table copy upstream makes per frame
/// (`ZSTD_resetCCtx_byCopyingCDict`). An attached dictionary stays resident in
/// tables of its own instead. The frame compressor restores on this and the
/// Fast reset keeps its table only on it, so the two cannot disagree.
pub(crate) fn restores_primed_snapshot(
    dict_use: Option<DictUse>,
    source_size: Option<u64>,
) -> bool {
    source_size.is_some_and(|size| size != crate::encoding::cparams::CONTENTSIZE_UNKNOWN)
        && matches!(dict_use, Some(DictUse::Copy | DictUse::Load))
}

/// How a greedy / lazy frame compressed with a dictionary takes its
/// match-finder from the dictionary's own cParams (upstream
/// `ZSTD_resetCCtx_usingCDict`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RowDictPlan {
    /// `ZSTD_shouldAttachDict`: the dictionary tables are searched in place
    /// (`dictMatchState`) instead of being copied into the frame's tables.
    pub(crate) attach: bool,
    /// The CDict's `useRowMatchFinder`, inherited by the frame.
    pub(crate) use_row: bool,
    /// The CDict's cParams: the geometry (`hash_log`, `chain_log`,
    /// `search_log` → row width) and key width its tables were built with.
    pub(crate) cdict: crate::encoding::cparams::CParams,
}

/// What a frame runs: its match finder and the strategy its block splitter
/// answers to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameParams {
    /// The match finder's parameters.
    pub(crate) params: LevelParams,
    /// The lazy backend's plan for a dictionary, on a greedy..btlazy2 match
    /// finder.
    pub(crate) row_plan: Option<RowDictPlan>,
    /// How the frame's dictionary enters the match finder; `None` without one.
    pub(crate) dict_use: Option<DictUse>,
    /// The strategy the level resolves to for this frame before a CDict's
    /// cParams replace it. Upstream decides the post-block splitter from it
    /// (`ZSTD_CCtx_init_compressStream2` resolves `postBlockSplitter` from the
    /// frame's own cParams, and `ZSTD_resetCCtx_usingCDict` leaves it alone).
    pub(crate) split_strategy: crate::encoding::strategy::StrategyTag,
}

/// [`resolve_level_params`] for a frame that compresses with a dictionary of
/// `dict_size` bytes. Upstream builds the CDict with
/// `ZSTD_getCParams(level, UNKNOWN, dictSize, createCDict)` and the frame
/// then runs the CDict's strategy / widths / search depth / match-finder:
/// re-adjusted to the source when the dictionary is attached
/// (`ZSTD_resetCCtx_byAttachingCDict`), verbatim when it is copied
/// (`ZSTD_resetCCtx_byCopyingCDict`); only the frame's own `windowLog` is
/// kept. The CDict's strategy is taken whatever backend family the plain
/// level resolved to; a lazy-band CDict (greedy..btlazy2) also carries the
/// lazy backend's [`RowDictPlan`]. The caller's `overrides` are part of the
/// CDict's cParams, as they are for a dictionary upstream loads into a context
/// that carries them.
pub(crate) fn resolve_level_params_with_dict(
    level: CompressionLevel,
    source_size: Option<u64>,
    sizes: crate::encoding::DictionarySizes,
    overrides: &crate::encoding::parameters::ParamOverrides,
) -> FrameParams {
    use crate::encoding::cparams::{
        CONTENTSIZE_UNKNOWN, attach_cparams, copy_cparams, get_cdict_cparams, should_attach_dict,
        uses_row_match_finder,
    };
    // A raw frame searches nothing, so a dictionary has no tier to give it.
    if sizes.content == 0 || matches!(level, CompressionLevel::Uncompressed) {
        let params = resolve_level_params(level, source_size);
        return FrameParams {
            params,
            row_plan: None,
            dict_use: None,
            split_strategy: params.strategy_tag,
        };
    }
    // The pledged-size sentinel is an unknown size, which attaches the
    // dictionary (`ZSTD_shouldAttachDict`), not a huge one that copies it.
    let source_size = source_size.filter(|&size| size != CONTENTSIZE_UNKNOWN);
    if dictionary_loads_into_frame(sizes, source_size, overrides.dictionary_attach) {
        return resolve_level_params_for_loaded_dict(level, source_size, sizes, overrides);
    }
    // The CDict's cParams decide the frame's strategy REGARDLESS of the
    // backend family the plain level resolved to for this source size
    // (upstream takes them unconditionally): L13 on a 4 KiB source is
    // btopt, but a 300 KiB CDict is btlazy2 and the frame runs btlazy2; L4
    // on a 1 MiB source is dfast, but a 4 KiB CDict is greedy. Only the
    // frame's own `windowLog` is kept.
    let cdict = get_cdict_cparams(numeric_level(level), sizes.serialized, overrides);
    // `ZSTD_shouldAttachDict`, bounded by the backend's attach representability:
    // the Fast / Dfast attached tables pack the dict position next to a tag, so
    // they index at most 2^24 content bytes. A larger dictionary is primed in
    // COPY mode, and the frame must then run the CDict's verbatim table
    // geometry (`byCopyingCDict`) — copying it into source-capped attach-mode
    // tables would collide away its matches.
    let attach_fits = match cdict.strategy {
        1 => sizes.content <= MAX_FAST_ATTACH_DICT_REGION,
        2 => sizes.content <= crate::encoding::dfast::DFAST_ATTACH_DICT_MAX_LEN,
        _ => true,
    };
    let attach =
        should_attach_dict(&cdict, source_size, overrides.dictionary_attach) && attach_fits;
    // The frame's own window: its level row and knobs for the source, with the
    // dictionary counted in the tier and the source log only when it is copied
    // (`ZSTD_getCParamMode`: `ZSTD_cpm_attachDict` drops the dictionary size,
    // `ZSTD_cpm_noAttachDict` keeps it, zstd_compress.c `ZSTD_getCParamRowSize`
    // and `ZSTD_adjustCParams_internal`). A copied dictionary sits in the
    // window, so the window covers it.
    let src = source_size.unwrap_or(CONTENTSIZE_UNKNOWN);
    let own = crate::encoding::cparams::get_frame_cparams(
        numeric_level(level),
        src,
        if attach { 0 } else { sizes.content },
        overrides,
    );
    let split_strategy = level_params_from_cparams(own).strategy_tag;
    let frame = if attach {
        attach_cparams(cdict, src, own.window_log, overrides.row_match_finder)
    } else {
        copy_cparams(cdict, own.window_log)
    };
    let params = level_params_from_cparams(frame);
    // Fast / dfast / optimal strategies: their backends prime the dictionary
    // themselves; no lazy-backend plan.
    let row_plan = (3..=6).contains(&cdict.strategy).then(|| RowDictPlan {
        attach,
        use_row: uses_row_match_finder(&cdict, overrides.row_match_finder),
        cdict,
    });
    FrameParams {
        params,
        row_plan,
        dict_use: Some(if attach {
            DictUse::Attach
        } else {
            DictUse::Copy
        }),
        split_strategy,
    }
}

/// The last [`resolve_level_params_with_dict`] answer, for a caller that asks
/// it again for every frame of the same dictionary: upstream resolves a CDict's
/// cParams once, when it builds the CDict, and a frame only adjusts them to its
/// source (`ZSTD_resetCCtx_usingCDict`).
#[derive(Clone, Copy, Default)]
pub(crate) struct DictParamsMemo(Option<(DictParamsKey, FrameParams)>);

/// Everything [`resolve_level_params_with_dict`] reads.
#[derive(Clone, Copy, PartialEq, Eq)]
struct DictParamsKey {
    level: CompressionLevel,
    source_size: Option<u64>,
    sizes: crate::encoding::DictionarySizes,
    overrides: crate::encoding::parameters::ParamOverrides,
}

impl DictParamsMemo {
    /// [`resolve_level_params_with_dict`], answered from the last call when it
    /// asked the same.
    pub(crate) fn resolve(
        &mut self,
        level: CompressionLevel,
        source_size: Option<u64>,
        sizes: crate::encoding::DictionarySizes,
        overrides: &crate::encoding::parameters::ParamOverrides,
    ) -> FrameParams {
        let key = DictParamsKey {
            level,
            source_size,
            sizes,
            overrides: *overrides,
        };
        if let Some((last, answer)) = &self.0
            && *last == key
        {
            return *answer;
        }
        let answer = resolve_level_params_with_dict(level, source_size, sizes, overrides);
        self.0 = Some((key, answer));
        answer
    }
}

/// [`resolve_level_params`] for a frame whose dictionary is loaded into tables
/// the frame resolved for its own source, rather than attached or copied from
/// the dictionary's own.
///
/// The dictionary's own preparation does not enter this: upstream resets the
/// context from the requested parameters for the real source, counting the
/// dictionary only as extra content the window has to cover
/// (`ZSTD_getCParams(level, srcSize, dictContentSize)`, zstd_compress.c:5839),
/// and then loads the dictionary into the tables that came out. So the tier is
/// the source's, not the dictionary's.
///
/// The caller's knobs are applied HERE rather than afterwards, because the
/// adjustment that bounds them has to run last: a `hashLog` the caller asks for
/// is capped at `dictAndWindowLog + 1`, and a cap applied before the knob it
/// bounds does nothing. Measured on the i9 with the cap out of order, a 1 KiB
/// window under an 18 KiB dictionary built a table of 807 KB and paid 832M
/// cycles over a 4 MiB source where the same frame without the dictionary pays
/// 178M: the window slides every kilobyte and each slide walks the whole table.
pub(crate) fn resolve_level_params_for_loaded_dict(
    level: CompressionLevel,
    source_size: Option<u64>,
    sizes: crate::encoding::DictionarySizes,
    overrides: &crate::encoding::parameters::ParamOverrides,
) -> FrameParams {
    use crate::encoding::cparams::{CONTENTSIZE_UNKNOWN, uses_row_match_finder};
    // `resolve_level_params_with_dict`, the one caller, answers an empty
    // dictionary and a raw frame before it asks this.
    debug_assert!(sizes.content != 0 && !matches!(level, CompressionLevel::Uncompressed));
    let frame = crate::encoding::cparams::get_frame_cparams(
        numeric_level(level),
        source_size.unwrap_or(CONTENTSIZE_UNKNOWN),
        sizes.content,
        overrides,
    );
    let params = level_params_from_cparams(frame);
    let row_plan = (3..=6).contains(&frame.strategy).then(|| RowDictPlan {
        // The dictionary lives in the frame's own tables, so there is no
        // separate set to search in place.
        attach: false,
        use_row: uses_row_match_finder(&frame, overrides.row_match_finder),
        cdict: frame,
    });
    // The frame runs the cParams it resolved for itself, the splitter's
    // strategy among them.
    FrameParams {
        params,
        row_plan,
        dict_use: Some(DictUse::Load),
        split_strategy: params.strategy_tag,
    }
}

/// The cheap fingerprint pre-splitter level for a compression level (the
/// C-like `blockSplitterLevel`), resolved through the same per-level
/// `LevelParams` table as every other tuning knob. `None` keeps the whole
/// 128 KiB block. The frame loop reads this instead of hardcoding the
/// level→split mapping at the call site.
pub(crate) fn level_pre_split(level: CompressionLevel) -> Option<usize> {
    // Resolve through `resolve_level_params` directly — NOT via the legacy
    // `numeric_level()` alias — so named presets read the SAME table row as
    // every other tuning knob (`Best` maps to its own row there, which is
    // not the row its numeric alias points at). `Uncompressed` (raw
    // blocks) never splits.
    if matches!(level, CompressionLevel::Uncompressed) {
        return None;
    }
    resolve_level_params(level, None)
        .pre_split()
        .map(usize::from)
}

#[cfg(test)]
mod tests;
