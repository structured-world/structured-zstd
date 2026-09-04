use alloc::vec::Vec;
use core::cmp::Ordering;

/// Cache primitive for `HuffmanTable::cached_encoded_weight_description`,
/// std-only. `std::sync::OnceLock` is `Sync` (atomic-init), so wrapping
/// it inside `pub struct HuffmanTable` keeps the type's auto-traits
/// intact for downstream consumers that share encoder tables across
/// threads. **The cache is entirely absent in no_std builds**: the
/// `cached_encoded_weight_description` field is `#[cfg(feature = "std")]`,
/// so `HuffmanTable` retains `Sync` unconditionally regardless of which
/// feature set the consumer builds with. no_std embedded targets that
/// might run `HuffmanTable` across threads (e.g. via `Arc`) lose the
/// per-table FSE-encode cache as a trade-off — they get the
/// recompute-every-time path that existed before the cache landed.
#[cfg(feature = "std")]
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
enum DescriptionState {
    /// Not encoded yet for the table's current contents.
    #[default]
    NotComputed,
    /// The FSE form was rejected, so the raw nibble description is what a
    /// writer emits. Recorded rather than recomputed: the rejection costs the
    /// same FSE encode as an acceptance.
    NotEncodable,
    /// The first `n` bytes of the buffer hold the encoding.
    Encoded(usize),
}

/// Cached FSE-encoded weight description, held with the table it describes.
///
/// The buffer outlives the contents: a table that is rebuilt or overwritten
/// resets the state but keeps the allocation, so a description costs one
/// allocation for the life of the table rather than one per build. That is the
/// same reasoning `clone_from` below applies to the code containers, and this
/// field used to be the exception, replaced wholesale on every rebuild and
/// every clone.
///
/// Plain data, so `HuffmanTable` stays `Sync` for consumers that share encoder
/// tables across threads.
#[cfg(feature = "std")]
#[derive(Default)]
struct WeightDescription {
    buf: Vec<u8>,
    state: DescriptionState,
}

#[cfg(feature = "std")]
impl WeightDescription {
    /// Bring the buffer to the largest size any description can need, once.
    ///
    /// Sizing it to each individual description instead makes it grow with
    /// whichever table it is reused across, and since descriptions vary in
    /// length that is a reallocation every time a longer one lands in a buffer
    /// last used by a shorter one. One weight per symbol bounds it, and the
    /// encoder writes at most that before the length gate rejects the result.
    fn reserve_to_bound(&mut self) {
        if self.buf.capacity() < MAX_HUFFMAN_ALPHABET {
            self.buf.reserve(MAX_HUFFMAN_ALPHABET - self.buf.len());
        }
    }
}

#[cfg(feature = "std")]
impl Clone for WeightDescription {
    fn clone(&self) -> Self {
        Self {
            buf: self.buf.clone(),
            state: self.state,
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.reserve_to_bound();
        self.buf.clone_from(&source.buf);
        self.state = source.state;
    }
}

use crate::{
    bit_io::BitWriter,
    fse::fse_encoder::{self, FSEEncoder},
    histogram,
};

/// Widest Huffman alphabet the format admits: one code per byte value. Every
/// per-symbol buffer here is bounded by it, which is what lets them sit on the
/// stack instead of being allocated per table.
const MAX_HUFFMAN_ALPHABET: usize = 256;

pub(crate) struct HuffmanEncoder<'output, 'table, V: AsMut<Vec<u8>>> {
    table: &'table HuffmanTable,
    writer: &'output mut BitWriter<V>,
}

impl<V: AsMut<Vec<u8>>> HuffmanEncoder<'_, '_, V> {
    pub fn new<'o, 't>(
        table: &'t HuffmanTable,
        writer: &'o mut BitWriter<V>,
    ) -> HuffmanEncoder<'o, 't, V> {
        HuffmanEncoder { table, writer }
    }

    /// Encodes the data using the provided table
    /// Writes
    /// * Table description
    /// * Encoded data
    /// * Padding bits to fill up last byte
    pub fn encode(&mut self, data: &[u8], with_table: bool) {
        if with_table {
            self.write_table();
        }
        Self::encode_stream(self.table, self.writer, data);
    }

    /// Encodes the data using the provided table in 4 concatenated streams.
    ///
    /// Upstream zstd-faithful port of `HUF_compress4X_usingCTable_internal_body`
    /// (`huf_compress.c:1169-1216`): emits the table description (if
    /// requested), a 6-byte jump-table placeholder, then four
    /// independent 1X Huffman bitstreams via the unrolled
    /// dual-container loop in [`Self::encode_one_stream`]. Stream
    /// sizes are patched back into the jump table after encoding.
    ///
    /// Upstream zstd's `HUF_compress1X_usingCTable_internal_body_loop` (lines
    /// 991-1043) extracts ILP by encoding `kUnroll` symbols into
    /// `bitContainer[0]` and another `kUnroll` symbols into
    /// `bitContainer[1]` in parallel, then merging — breaking the
    /// data dependency that a single accumulator would impose. We
    /// mirror that here via `HufCStream::add_bits<FAST=true>` +
    /// `zero_index1` / `merge_index1`.
    ///
    /// `kUnroll` choice mirrors upstream zstd's `tableLog`-driven dispatch
    /// (`huf_compress.c:1077-1108`) so each instantiation keeps the
    /// container's per-symbol nb_bits sum below 64 - 4 (the upstream zstd "≥
    /// 4 free bits" precondition for `kFast=1`).
    pub fn encode4x(&mut self, data: &[u8], with_table: bool) {
        assert!(
            data.len() >= 12,
            "upstream zstd HUF_compress4X requires srcSize >= 12"
        );

        if with_table {
            self.write_table();
        }

        // Upstream zstd's jump table: 3 × u16 LE sizes (size1, size2, size3);
        // size4 is implicit from total - sum. Reserve 6 bytes here,
        // patch in after each stream finishes.
        let jt_bit_idx = self.writer.index();
        self.writer.write_bits(0u16, 16);
        self.writer.write_bits(0u16, 16);
        self.writer.write_bits(0u16, 16);

        // Upstream zstd: `segmentSize = (srcSize + 3) / 4` for the first 3
        // segments; last segment takes whatever remains.
        let segment_size = data.len().div_ceil(4);
        let segments = [
            &data[..segment_size],
            &data[segment_size..segment_size * 2],
            &data[segment_size * 2..segment_size * 3],
            &data[segment_size * 3..],
        ];

        let table_log = self.table.table_log();
        let packed_codes = self.table.packed_codes();
        let mut stream_sizes = [0u16; 3];

        for (i, segment) in segments.iter().enumerate() {
            let bytes_written = self.writer.with_aligned_output_mut(|output| {
                let dst_capacity = Self::huf_tight_compress_bound(segment.len(), table_log);
                // `HufCStream::new` returns `None` only when
                // `dst_capacity <= 8` (8-byte flush slack would have
                // nowhere to go). Capacity itself is reserved
                // inside `new` via `Vec::reserve(dst_capacity)`, so
                // there is no "output buffer too small" failure mode
                // — the only way to trip this is the upstream
                // `huf_tight_compress_bound` returning ≤ 8 bytes,
                // which it never does for a non-empty segment under
                // `table_log >= 1` (formula: src*table_log/8 + 24).
                let mut bit_c = super::huf_cstream::HufCStream::new(output, dst_capacity).expect(
                    "HufCStream::new returned None — dst_capacity (from \
                         huf_tight_compress_bound) must be > 8 for a non-empty segment",
                );
                Self::encode_one_stream(packed_codes, &mut bit_c, segment, table_log);
                bit_c.close()
            });
            // Upstream zstd `HUF_CStream_close` returns 0 on overflow (the
            // dst_capacity was exhausted before close completed) and
            // truncates the output back to the segment start. Our
            // `huf_tight_compress_bound` is sized exactly per upstream zstd's
            // spec, so this branch is unreachable under a correctly
            // sized buffer — but if a future change ever mis-sizes
            // the bound, silently emitting a 0-length stream would
            // produce an invalid jump table that decompresses to
            // garbage. Panic loudly instead.
            assert!(
                bytes_written > 0,
                "HufCStream::close overflowed dst_capacity for segment {i}; \
                 huf_tight_compress_bound is undersized for table_log={table_log}",
            );
            if i < 3 {
                assert!(
                    bytes_written <= u16::MAX as usize,
                    "Huffman stream exceeded 64 KiB jump-table limit",
                );
                stream_sizes[i] = bytes_written as u16;
            }
        }

        self.writer.change_bits(jt_bit_idx, stream_sizes[0], 16);
        self.writer
            .change_bits(jt_bit_idx + 16, stream_sizes[1], 16);
        self.writer
            .change_bits(jt_bit_idx + 32, stream_sizes[2], 16);
    }

    /// Upstream zstd `HUF_tightCompressBound(srcSize, tableLog)` from
    /// `huf_compress.c:1050-1053`: an upper bound on encoded bytes
    /// that lets the hot path skip per-symbol bounds checks. We add
    /// 16 extra slack for the trailing end-mark byte + close-flush
    /// overshoot.
    #[inline(always)]
    fn huf_tight_compress_bound(src_size: usize, table_log: u32) -> usize {
        ((src_size * table_log as usize) >> 3) + 8 + 16
    }

    /// Dispatch on table_log to pick `kUnroll`, `kFastFlush`,
    /// `kLastFast` matching upstream zstd's 64-bit branch in
    /// `huf_compress.c:1092-1110`.
    fn encode_one_stream(
        table: &[u64],
        bit_c: &mut super::huf_cstream::HufCStream<'_>,
        data: &[u8],
        table_log: u32,
    ) {
        // Upstream zstd's fallback for tableLog > 11 OR insufficient dst
        // capacity: kUnroll=4 (on 64-bit), kFast=0, kLastFast=0.
        // We always reserve `huf_tight_compress_bound` worth of dst,
        // so the dst-capacity branch never fires; only tableLog > 11
        // routes here, which clevels.h does not produce for L1-L22
        // Fast/DFast workloads but is correct to handle defensively.
        match table_log {
            11 => Self::encode_one_stream_unrolled::<5, true, false>(table, bit_c, data),
            10 => Self::encode_one_stream_unrolled::<5, true, true>(table, bit_c, data),
            9 => Self::encode_one_stream_unrolled::<6, true, false>(table, bit_c, data),
            8 => Self::encode_one_stream_unrolled::<7, true, false>(table, bit_c, data),
            7 => Self::encode_one_stream_unrolled::<8, true, false>(table, bit_c, data),
            // tableLog ∈ {1..=6, 12} — upstream zstd's "default" branch falls
            // here too. kUnroll=4 keeps per-flush bit budget safe.
            _ => Self::encode_one_stream_unrolled::<4, false, false>(table, bit_c, data),
        }
    }

    /// Upstream zstd `HUF_compress1X_usingCTable_internal_body_loop`
    /// (`huf_compress.c:991-1043`). Three phases:
    ///
    /// 1. Encode `n % K_UNROLL` symbols (slow / `FAST=false`) to
    ///    align to a `K_UNROLL` boundary, then flush.
    /// 2. If still not aligned to `2 * K_UNROLL`, encode another
    ///    `K_UNROLL` symbols (with the last using `K_LAST_FAST`),
    ///    then flush.
    /// 3. Main loop: encode `K_UNROLL` symbols into stream 0, flush;
    ///    `zero_index1`, encode `K_UNROLL` symbols into stream 1,
    ///    merge into stream 0, flush. Loop processes `2 * K_UNROLL`
    ///    input symbols per iteration with the two parallel
    ///    sub-streams breaking the dependency chain.
    ///
    /// Symbols are consumed in REVERSE (`data[n-1]` then `data[n-2]`
    /// …) matching upstream zstd's `ip[--n]` cadence — the resulting
    /// bitstream is decoded forward, but encoding right-to-left lets
    /// upstream zstd pack high-frequency low-bit codes against the top of
    /// the container.
    ///
    /// `K_FAST_FLUSH`: skip the post-write overflow clamp in
    /// `flush_bits`. Safe whenever the output buffer was sized via
    /// `huf_tight_compress_bound`.
    /// `K_LAST_FAST`: use `FAST=true` for the final `add_bits` of
    /// each unroll group. Safe when `K_UNROLL * max_nb_bits + 4 <=
    /// 64`, i.e. `K_UNROLL <= (60 / table_log)`.
    fn encode_one_stream_unrolled<
        const K_UNROLL: usize,
        const K_FAST_FLUSH: bool,
        const K_LAST_FAST: bool,
    >(
        table: &[u64],
        bit_c: &mut super::huf_cstream::HufCStream<'_>,
        data: &[u8],
    ) {
        // Delegate to the hoisted-locals encode loop on `HufCStream`,
        // which keeps the bit containers / positions / cursor
        // register-resident (upstream zstd's `HUF_CStream_t` shape) instead of
        // reloading them from `*bit_c` per symbol. Byte-identical output.
        bit_c.encode_unrolled::<K_UNROLL, K_FAST_FLUSH, K_LAST_FAST>(table, data);
    }

    /// Encode one stream and pad it to fill the last byte
    fn encode_stream<VV: AsMut<Vec<u8>>>(
        table: &HuffmanTable,
        writer: &mut BitWriter<VV>,
        data: &[u8],
    ) {
        for symbol in data.iter().rev() {
            let (code, num_bits) = table.codes[*symbol as usize];
            debug_assert!(num_bits > 0);
            writer.write_bits(code, num_bits as usize);
        }

        let bits_to_fill = writer.misaligned();
        if bits_to_fill == 0 {
            writer.write_bits(1u32, 8);
        } else {
            writer.write_bits(1u32, bits_to_fill);
        }
    }

    /// Per-symbol weights into a caller's buffer; see
    /// [`HuffmanTable::weights_into`].
    fn weights_into<'b>(&self, buf: &'b mut [u8; MAX_HUFFMAN_ALPHABET]) -> &'b [u8] {
        self.table.weights_into(buf)
    }

    /// Owning form of [`Self::weights_into`], for tests that want the weights
    /// as a value. Production paths use the buffer form: this allocates.
    #[cfg(test)]
    pub(super) fn weights(&self) -> Vec<u8> {
        let mut buf = [0u8; MAX_HUFFMAN_ALPHABET];
        self.weights_into(&mut buf).to_vec()
    }

    fn write_table(&mut self) {
        #[cfg(feature = "std")]
        {
            // Cached path: the size query that precedes every emit has already
            // encoded the description, so this reads it. A hit emits the FSE
            // bytes; a recorded rejection emits the raw nibbles, which the
            // cache does not hold and so are recomputed here.
            match self.table.weight_description_state() {
                DescriptionState::Encoded(len) => {
                    let fse_description = &self.table.cached_encoded_weight_description.buf[..len];
                    self.writer.write_bits(fse_description.len() as u8, 8);
                    self.writer.append_bytes(fse_description);
                    return;
                }
                DescriptionState::NotEncodable => {
                    let mut buf = [0u8; MAX_HUFFMAN_ALPHABET];
                    let weights = self.weights_into(&mut buf);
                    let weights = &weights[..weights.len() - 1];
                    Self::write_raw_weight_description(self.writer, weights);
                    return;
                }
                DescriptionState::NotComputed => {}
            }
            // Cold path, reached only by a writer whose table was never asked
            // for its description size. Nothing to fill in place through a
            // shared borrow, so this one encodes into a local buffer and drops
            // it; `weights` is computed once and shared between the FSE encode
            // and the raw fallback, which would otherwise recompute the slice
            // (a measurable hotspot for small / low-cardinality tables).
            let mut buf = [0u8; MAX_HUFFMAN_ALPHABET];
            let weights = self.weights_into(&mut buf);
            let weights = &weights[..weights.len() - 1];
            let mut encoded = Vec::new();
            if Self::encode_weight_description_into(weights, &mut encoded) {
                self.writer.write_bits(encoded.len() as u8, 8);
                self.writer.append_bytes(&encoded);
            } else {
                Self::write_raw_weight_description(self.writer, weights);
            }
        }
        #[cfg(not(feature = "std"))]
        {
            // no_std: no cache field, no shared state — single weight
            // computation, branch on FSE-vs-raw based on direct encoder call.
            let mut buf = [0u8; MAX_HUFFMAN_ALPHABET];
            let weights = self.weights_into(&mut buf);
            let len = weights.len();
            let weights = &buf[..len - 1];
            let mut encoded = Vec::new();
            if Self::encode_weight_description_into(weights, &mut encoded) {
                self.writer.write_bits(encoded.len() as u8, 8);
                self.writer.append_bytes(&encoded);
            } else {
                Self::write_raw_weight_description(self.writer, weights);
            }
        }
    }

    /// Encodes Huffman weights using FSE when that representation is valid and beneficial.
    ///
    /// Returns `None` when FSE metadata is not suitable, so callers fall back to raw weight encoding.
    /// Fill `encoded` with the FSE weight description, reporting whether that
    /// form is the one to emit. `encoded` is the caller's buffer so a table
    /// rebuilt many times per frame reuses one allocation; on `false` its
    /// contents are meaningless and the raw nibble description is written
    /// instead.
    fn encode_weight_description_into(weights: &[u8], encoded: &mut Vec<u8>) -> bool {
        encoded.clear();
        if weights.len() <= 2 {
            return false;
        }

        // Upstream zstd `HUF_compressWeights` early-outs
        // (huf_compress.c:162-167), applied from the weight histogram BEFORE
        // building any FSE table: a single distinct weight value (`max_count ==
        // len`) is an RLE stream FSE cannot represent, and a stream where every
        // value occurs at most once (`max_count == 1`) does not compress. Both
        // fall back to the raw nibble description. Skipping the FSE encode for
        // these cases avoids producing a stream the decoder would reject (the
        // failure the former decode round-trip existed to catch) — the
        // single-distinct-weight case is the common uniform-literal frame.
        let mut counts = [0usize; 13];
        for &weight in weights {
            counts[weight as usize] += 1;
        }
        let max_count = counts.iter().copied().max().unwrap_or(0);
        if max_count == weights.len() || max_count <= 1 {
            return false;
        }

        // Reserve to the weight count: the FSE-encoded description is rejected
        // above `weights.len() / 2` (the raw nibble fallback wins) and at 128
        // bytes outright, so `weights.len()` is a generous one-shot capacity
        // that keeps the BitWriter's backing buffer from reallocating as the
        // interleaved stream is written. A reused buffer is already at that
        // size after its first fill, so this reserves nothing on later calls.
        encoded.reserve(weights.len());
        {
            let mut writer = BitWriter::from(&mut *encoded);
            let mut encoder = FSEEncoder::new(
                fse_encoder::build_table_from_symbol_counts(&counts, 6, false),
                &mut writer,
            );
            encoder.encode_interleaved(weights);
            writer.flush();
        }

        let raw_description_is_representable = weights.len() <= 128;
        // Upstream zstd `HUF_writeCTable_wksp` (huf_compress.c:276) keeps the FSE
        // weight description ONLY when `hSize < maxSymbolValue/2` (FLOOR). Our
        // `weights.len()` equals `maxSymbolValue` (the implicit last symbol is not
        // written; see `write_raw_weight_description`), so the threshold is
        // `weights.len() / 2`. `div_ceil` was 1 byte too permissive on odd symbol
        // counts: it kept the FSE description in a boundary band where upstream
        // emits the cheap direct nibbles, producing a frame that is fractionally
        // smaller but markedly slower to decode (the decoder pays an FSE
        // weight-table build + FSE weight decode instead of a nibble unpack).
        let raw_description_bytes = weights.len() / 2;
        if encoded.len() > 1
            && (encoded.len() < raw_description_bytes || !raw_description_is_representable)
        {
            if encoded.len() >= 128 {
                return false;
            }
            // Trust the FSE encoding and emit it directly, matching upstream
            // zstd's HUF_writeCTable (no decode round-trip). The upstream zstd
            // early-outs above guarantee FSE is only attempted on streams it
            // can represent, so the emitted description always decodes back —
            // verified exhaustively by `fse_weight_descriptions_roundtrip` over
            // a wide alphabet sweep. The former per-call decode + re-encode
            // verification was a dominant per-frame heap churn on tiny
            // dict-compress frames, amplified by the musl allocator.
            true
        } else {
            false
        }
    }

    /// Writes the raw nibble-packed Huffman weight representation.
    fn write_raw_weight_description<VV: AsMut<Vec<u8>>>(
        writer: &mut BitWriter<VV>,
        weights: &[u8],
    ) {
        assert!(weights.len() <= 128);
        writer.write_bits(weights.len() as u8 + 127, 8);
        let (pairs, remainder) = weights.as_chunks::<2>();
        for &[weight1, weight2] in pairs {
            assert!(weight1 < 16);
            assert!(weight2 < 16);
            writer.write_bits(weight2, 4);
            writer.write_bits(weight1, 4);
        }
        if !remainder.is_empty() {
            let weight = remainder[0];
            assert!(weight < 16);
            writer.write_bits(weight << 4, 8);
        }
    }
}

pub struct HuffmanTable {
    /// Index is the symbol, values are the bitstring in the lower bits of the u32 and the amount of bits in the u8
    codes: Vec<(u32, u8)>,
    /// Upstream zstd-format packed Huffman codes (`HUF_CElt`): one `u64` per
    /// symbol where the bottom 8 bits hold `nb_bits` and the top
    /// `(64 - nb_bits)` bits hold `value` left-shifted to the high
    /// end. Built in lockstep with `codes` so the upstream zstd-style
    /// dual-container [`super::huf_cstream::HufCStream`] can index
    /// symbols with a single u64 load (no per-symbol shift+combine).
    /// See `huf_compress.c:208-221` for the upstream zstd format.
    packed_codes: Vec<u64>,
    /// Active Huffman table-log (1..=12). Stored explicitly so
    /// `encode4x_reference` can dispatch to the correct `kUnroll`
    /// template instantiation without re-scanning `codes`.
    table_log: u32,
    /// Lazy cache of the FSE-encoded weight description. Avoids re-running
    /// `encode_weight_description` across `try_table_description_size` and
    /// `write_table` for the same table instance. **std-only** —
    /// `core::cell::OnceCell` is `!Sync` and would break the `Sync`
    /// auto-trait for `pub HuffmanTable` in no_std builds; no_std users
    /// keep the original recompute-every-time semantics. See the
    /// `CachedDescription` type-alias doc above for full rationale.
    #[cfg(feature = "std")]
    cached_encoded_weight_description: WeightDescription,
}

/// Manual impl so `clone_from` reuses the destination's existing `Vec`
/// buffers; the derived version falls back to `*self = source.clone()`,
/// re-allocating both code containers on every per-frame entropy seed and
/// per-block rollback snapshot.
impl Clone for HuffmanTable {
    fn clone(&self) -> Self {
        Self {
            codes: self.codes.clone(),
            packed_codes: self.packed_codes.clone(),
            table_log: self.table_log,
            #[cfg(feature = "std")]
            cached_encoded_weight_description: self.cached_encoded_weight_description.clone(),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.codes.clone_from(&source.codes);
        self.packed_codes.clone_from(&source.packed_codes);
        self.table_log = source.table_log;
        #[cfg(feature = "std")]
        {
            // `clone_from`, not an assignment: assigning drops the
            // destination's buffer and takes a new one for the source's
            // contents, which is the allocation this whole type exists to
            // avoid. Same reasoning as the two code containers above.
            self.cached_encoded_weight_description
                .clone_from(&source.cached_encoded_weight_description);
        }
    }
}

/// Measurement-only toggle (gated behind `bench-internals`): when set, every
/// [`HuffmanTable::build_from_counts`] takes the cheap single-build path
/// instead of the #167 table-log search, so a bench harness can A/B the search
/// on/off from a single build. Never set in shipping code.
#[cfg(feature = "bench-internals")]
pub static FORCE_CHEAP_HUF: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Set the [`FORCE_CHEAP_HUF`] measurement toggle.
#[cfg(feature = "bench-internals")]
pub fn set_force_cheap_huf(on: bool) {
    FORCE_CHEAP_HUF.store(on, core::sync::atomic::Ordering::Relaxed);
}

impl HuffmanTable {
    /// Heap bytes this table holds: the per-symbol code table, the packed
    /// dual-container codes, and the encoded weight description once it has
    /// been built.
    ///
    /// The description is not a transient. It is built lazily but then lives as
    /// long as the table does, and tables outlive the block that made them —
    /// the emitter carries one between blocks and the builder parks a spare —
    /// so its bytes are retained and a caller budgeting around a context has to
    /// see them.
    pub fn heap_size(&self) -> usize {
        // Capacity, not length: the buffer is kept across rebuilds precisely so
        // it outlives any one description, and a caller budgets against what is
        // held rather than what is currently in use.
        #[cfg(feature = "std")]
        let cached = self.cached_encoded_weight_description.buf.capacity();
        #[cfg(not(feature = "std"))]
        let cached = 0;
        self.codes.capacity() * core::mem::size_of::<(u32, u8)>()
            + self.packed_codes.capacity() * core::mem::size_of::<u64>()
            + cached
    }

    pub fn build_from_data(data: &[u8]) -> Self {
        let mut counts = [0; 256];
        let (max_symbol, _) = histogram::count_bytes(data, &mut counts);

        Self::build_from_counts(&counts[..=max_symbol])
    }

    /// Build the literals Huffman table, running the #167 table-log search
    /// only when `use_search` is set; otherwise take the cheap single-build
    /// (the upstream non-optimalDepth path). The caller gates `use_search` by
    /// strategy and source size (see `CompressState::huf_optimal_search`).
    pub fn build_from_counts_gated(counts: &[usize], use_search: bool) -> Self {
        let mut scratch = WeightScratch::default();
        Self::build_from_counts_gated_with(counts, use_search, &mut scratch)
    }

    /// [`Self::build_from_counts_gated`] reusing caller-owned scratch. The
    /// cheap path builds a tree and two weight buffers per call, and a
    /// compressor calls it once per block — and once per split candidate where
    /// the block splitter probes — so taking them fresh every time was the
    /// single largest source of per-frame allocations.
    pub(crate) fn build_from_counts_gated_with(
        counts: &[usize],
        use_search: bool,
        scratch: &mut WeightScratch,
    ) -> Self {
        if use_search {
            Self::build_from_counts(counts)
        } else {
            // Match upstream's cheap path: tableLog = FSE_optimalTableLog(11,
            // srcSize, maxSV, minus=1) (huf_compress.c:1286), height-limit to it,
            // not the raw natural height (11) which can cost a few bytes vs C.
            build_limited_weights_into(counts, cheap_huf_table_log(counts), scratch);
            let spare = scratch.spare_table.take();
            Self::build_from_weights_reusing(&scratch.weights, spare)
        }
    }

    pub fn build_from_counts(counts: &[usize]) -> Self {
        assert!(counts.len() <= 256);
        let symbol_cardinality = counts.iter().filter(|&&count| count > 0).count();
        if symbol_cardinality <= 1 {
            return Self::build_from_weights(&build_limited_weights(counts, 11));
        }
        // Measurement-only: force the cheap single-build path (the upstream
        // non-optimalDepth path) so a bench harness can compare #167 on/off
        // ratio and speed across levels from one build. Off in every shipping
        // build (gated behind `bench-internals`).
        #[cfg(feature = "bench-internals")]
        if FORCE_CHEAP_HUF.load(core::sync::atomic::Ordering::Relaxed) {
            return Self::build_from_weights(&build_limited_weights(
                counts,
                cheap_huf_table_log(counts),
            ));
        }

        let min_table_log = symbol_cardinality.ilog2() as usize + 1;
        let mut best_size = usize::MAX - 1;
        // Reused across every `table_log` candidate so the search allocates
        // nothing per iteration: `work` is the height-limit scratch, `cand`
        // holds the current candidate's weights, `best` retains the winning
        // weights — swapped in on a new best, so the loser's buffer recycles
        // back into `cand` for the next iteration. On a small alphabet
        // `min_table_log` is low, so the candidate count (and the old
        // per-candidate `leaves.to_vec()` + weight-`Vec` allocations) was the
        // dominant small-frame entropy-build cost.
        // Pre-size the reused scratch to its proven bound (alphabet <= 256, tree
        // <= 2*256-1 nodes) so the first candidate's resize/extend allocates once
        // and never reallocates across the table-log search.
        let mut work: Vec<HuffNode> = Vec::with_capacity(2 * counts.len());
        let mut cand: Vec<usize> = Vec::with_capacity(counts.len());
        let mut best: Vec<usize> = Vec::with_capacity(counts.len());
        let mut best_found = false;

        // Outer-loop scoring uses [`cheap_desc_size_proxy`] — an integer
        // entropy estimate of the weight description, no FSE encode.
        // Upstream zstd `HUF_writeCTable_wksp` picks the smaller of FSE / raw
        // serializations; the proxy mirrors that decision analytically.
        // Empirically (#167 validation sweep across the compare_ffi
        // matrix) the proxy preserves the `(table_log → total_size)`
        // minimum vs the exact `try_table_description_size` — so
        // selection is identical while the per-candidate FSE-encode
        // cost is gone. Issue: #167.
        //
        // Stack-allocated `weights_u8` buffer (256 B — counts.len() max
        // = 256) absorbs the per-candidate `Vec<usize> → &[u8]`
        // conversion that the proxy wants.
        let mut weights_u8 = [0u8; 256];
        // The Huffman tree shape (and thus the per-leaf natural depths) does
        // not depend on `table_log`; only the height limiting does. Build the
        // leaves once and reuse them across every candidate `table_log`
        // instead of rebuilding the whole tree per iteration.
        let leaves = build_huffman_leaf_depths(counts);
        for table_log in min_table_log..=11 {
            if !limited_weights_into(&leaves, counts.len(), table_log, &mut work, &mut cand) {
                cand = legacy_distributed_weights(counts);
            }
            let weights = &cand;
            if !huffman_weight_sum_is_power_of_two(weights) {
                continue;
            }
            // Per-symbol code length is `wtable_log + 1 - weight` (weight > 0) —
            // exactly what `build_from_weights` writes into `codes[..].bits`.
            // Derive `wtable_log`, `max_bits`, and the count-weighted size
            // estimate analytically from `weights` so the candidate search
            // never builds (then discards) a full encoder table; only the
            // winning weights are built into a table after the loop. The
            // `huffman_weight_sum_is_power_of_two` check above guarantees
            // `weight_sum` is a power of two, matching `build_from_weights`.
            let weight_sum: usize = weights
                .iter()
                .copied()
                .filter(|&w| w > 0)
                .map(|w| 1usize << (w - 1))
                .sum();
            let wtable_log = highest_bit_set(weight_sum) - 1;
            // `unwrap_or(1)` is the tightest safe default: weight 1 is the
            // longest valid code length, so `max_bits` can never exceed a real
            // code length even if the power-of-two guard were relaxed (a `0`
            // default would give `max_bits = wtable_log + 1`, larger than any
            // real length, and risk underflow). A positive weight always exists
            // here in practice (the `huffman_weight_sum_is_power_of_two` guard).
            let min_positive_weight = weights
                .iter()
                .copied()
                .filter(|&w| w > 0)
                .min()
                .unwrap_or(1);
            let max_bits = wtable_log + 1 - min_positive_weight;
            if max_bits < table_log && table_log > min_table_log {
                break;
            }
            // Upstream zstd `HUF_writeCTable` serializes `weights[..len-1]` — the
            // decoder reconstructs the final weight from the Kraft-
            // equality (sum of `2^(weight-1)` is a power of two). Pass
            // the same trimmed slice to the proxy so it scores the
            // *serialized* description, not the full table.
            let trimmed_len = weights.len().saturating_sub(1);
            for (slot, &w) in weights_u8[..trimmed_len].iter_mut().zip(weights.iter()) {
                debug_assert!(w <= u8::MAX as usize);
                *slot = w as u8;
            }
            let trimmed = &weights_u8[..trimmed_len];
            // Cheap proxy. If `None`, the candidate would not serialize
            // either as FSE or raw — skip it; the caller validates the
            // chosen table with `writeable_table_description_size` and
            // falls back to raw literals if needed.
            let Some(desc_size) = cheap_desc_size_proxy(trimmed) else {
                continue;
            };
            // Mirrors `estimate_compressed_size_from_counts`: per-symbol
            // `count * nb_bits` summed, then byte-rounded. `nb_bits` is the
            // same `wtable_log + 1 - weight` the built table would carry.
            // Plain `+`: estimate + table description, both bounded by the
            // block size — no overflow.
            let estimate_bits: usize = weights
                .iter()
                .zip(counts.iter())
                .filter(|&(&w, _)| w > 0)
                .map(|(&w, &count)| (wtable_log + 1 - w) * count)
                .sum();
            let payload = estimate_bits.div_ceil(8) + usize::from(estimate_bits.is_multiple_of(8));
            let new_size = payload + desc_size;
            if new_size > best_size + 1 {
                break;
            }
            if new_size < best_size {
                best_size = new_size;
                // Keep the winning weights without a clone: swap them into
                // `best`; the previous best's buffer lands in `cand` and is
                // recycled (cleared + refilled) by the next candidate.
                core::mem::swap(&mut best, &mut cand);
                best_found = true;
            }
        }

        if best_found {
            Self::build_from_weights(&best)
        } else {
            Self::build_from_weights(&build_limited_weights(counts, 11))
        }
    }

    /// Estimates encoded payload size in bytes for `data` using this table.
    pub(crate) fn estimate_compressed_size(&self, data: &[u8]) -> Option<usize> {
        let mut bits = 0usize;
        for &symbol in data {
            let (_, num_bits) = *self.codes.get(symbol as usize)?;
            if num_bits == 0 {
                return None;
            }
            bits += num_bits as usize;
        }
        let bytes = bits.div_ceil(8);
        Some(bytes + usize::from(bits.is_multiple_of(8)))
    }

    /// Returns exact writable table-description size when representable.
    /// std build path: consults the lazy cache to avoid re-encoding the
    /// weight stream when both planner and emitter call this for the
    /// same table. no_std build path: recomputes via the direct encoder
    /// every call (cache field absent — preserves `Sync`).
    pub(crate) fn try_table_description_size(&mut self) -> Option<usize> {
        #[cfg(feature = "std")]
        {
            // Encodes on the first call for these contents and caches it, so
            // the writer that follows reads rather than repeats the work. This
            // is also where the caching happens at all: it is the only step in
            // the emit path holding the table mutably.
            self.fill_weight_description_from_codes();
            if let Some(fse_description) = self.cached_encoded_weight_description() {
                return Some(fse_description.len() + 1);
            }
            let raw_weights_len = self.codes.len().saturating_sub(1);
            if raw_weights_len <= 128 {
                Some(raw_weights_len.div_ceil(2) + 1)
            } else {
                None
            }
        }
        #[cfg(not(feature = "std"))]
        {
            let mut buf = [0u8; MAX_HUFFMAN_ALPHABET];
            let weights = self.weights_into(&mut buf);
            let len = weights.len();
            let weights = &buf[..len - 1];
            let mut encoded = Vec::new();
            if HuffmanEncoder::<Vec<u8>>::encode_weight_description_into(weights, &mut encoded) {
                return Some(encoded.len() + 1);
            }
            if weights.len() <= 128 {
                Some(weights.len().div_ceil(2) + 1)
            } else {
                None
            }
        }
    }

    /// Alias for `try_table_description_size` used by call sites that require explicit writeability.
    pub(crate) fn writeable_table_description_size(&mut self) -> Option<usize> {
        self.try_table_description_size()
    }

    /// Owning form of [`Self::weights_into`], for tests that want the weights
    /// as a value. Production paths use the buffer form: this allocates.
    #[cfg(test)]
    fn weights(&self) -> Vec<u8> {
        let mut buf = [0u8; MAX_HUFFMAN_ALPHABET];
        self.weights_into(&mut buf).to_vec()
    }

    /// Per-symbol weights written into a caller's buffer. The alphabet is at
    /// most 256 symbols, so a stack buffer covers every table — and this runs
    /// once per built table, which is once per block and once per split
    /// candidate, so a `Vec` here was an allocation on that whole path.
    fn weights_into<'b>(&self, buf: &'b mut [u8; MAX_HUFFMAN_ALPHABET]) -> &'b [u8] {
        let max = *self.codes.iter().map(|(_, nb)| nb).max().unwrap();
        for (slot, &(_, nb)) in buf.iter_mut().zip(self.codes.iter()) {
            *slot = if nb == 0 { 0 } else { max - nb + 1 };
        }
        &buf[..self.codes.len()]
    }

    /// Encode the description if it has not been encoded for these contents
    /// yet. Takes `&mut self` because filling reuses the table's own buffer,
    /// which is the whole point: the previous lazy form populated through
    /// `&self` and so had to hand the cache a freshly allocated one.
    #[cfg(feature = "std")]
    fn fill_weight_description(&mut self, weights: &[u8]) {
        if self.cached_encoded_weight_description.state != DescriptionState::NotComputed {
            return;
        }
        let cache = &mut self.cached_encoded_weight_description;
        cache.reserve_to_bound();
        cache.state =
            if HuffmanEncoder::<Vec<u8>>::encode_weight_description_into(weights, &mut cache.buf) {
                DescriptionState::Encoded(cache.buf.len())
            } else {
                DescriptionState::NotEncodable
            };
    }

    #[cfg(feature = "std")]
    fn fill_weight_description_from_codes(&mut self) {
        let mut buf = [0u8; MAX_HUFFMAN_ALPHABET];
        // The returned slice borrows `buf`, not `self`, so the shared borrow
        // ends here and the fill below can take `&mut self`.
        let weights = self.weights_into(&mut buf);
        let len = weights.len();
        let weights = &buf[..len - 1];
        self.fill_weight_description(weights);
    }

    /// The cached encoding, or `None` when the FSE form was rejected OR has not
    /// been encoded yet. Callers that must tell those apart read
    /// [`Self::weight_description_state`].
    #[cfg(feature = "std")]
    fn cached_encoded_weight_description(&self) -> Option<&[u8]> {
        match self.cached_encoded_weight_description.state {
            DescriptionState::Encoded(len) => {
                Some(&self.cached_encoded_weight_description.buf[..len])
            }
            _ => None,
        }
    }

    #[cfg(feature = "std")]
    fn weight_description_state(&self) -> DescriptionState {
        self.cached_encoded_weight_description.state
    }

    /// Estimates encoded payload size in bytes directly from per-symbol counts.
    pub(crate) fn estimate_compressed_size_from_counts(&self, counts: &[usize]) -> usize {
        let bits = self
            .codes
            .iter()
            .zip(counts.iter())
            .map(|(&(_, bits), &count)| bits as usize * count)
            .sum::<usize>();
        bits.div_ceil(8) + usize::from(bits.is_multiple_of(8))
    }

    pub fn build_from_weights(weights: &[usize]) -> Self {
        Self::build_from_weights_reusing(weights, None)
    }

    /// [`Self::build_from_weights`] filling a discarded table's buffers instead
    /// of taking new ones. A table is built per block and, where the splitter
    /// probes, per split candidate; most are measured and thrown away, so
    /// handing the last one back turns two allocations per build into none.
    pub(crate) fn build_from_weights_reusing(weights: &[usize], spare: Option<Self>) -> Self {
        let weight_sum = weights
            .iter()
            .copied()
            .filter(|&weight| weight > 0)
            .map(|weight| 1 << (weight - 1))
            .sum::<usize>();
        if !weight_sum.is_power_of_two() {
            panic!("This is an internal error");
        }
        let table_log = highest_bit_set(weight_sum) - 1;
        let mut table = match spare {
            Some(mut spare) => {
                // Both buffers are overwritten in full below, so the recycled
                // contents do not survive; only their allocations do.
                spare.codes.clear();
                spare.codes.resize(weights.len(), (0, 0));
                spare.packed_codes.clear();
                spare.packed_codes.resize(weights.len(), 0u64);
                spare.table_log = table_log as u32;
                #[cfg(feature = "std")]
                {
                    // The contents describe the old codes and are stale, but
                    // the buffer they sat in is exactly what this path exists
                    // to keep: reset the state, not the allocation.
                    spare.cached_encoded_weight_description.state = DescriptionState::NotComputed;
                }
                spare
            }
            None => HuffmanTable {
                codes: alloc::vec![(0, 0); weights.len()],
                packed_codes: alloc::vec![0u64; weights.len()],
                table_log: table_log as u32,
                #[cfg(feature = "std")]
                cached_encoded_weight_description: WeightDescription::default(),
            },
        };
        let mut nb_per_rank = [0u16; 13];
        for &weight in weights {
            if weight > 0 {
                let nb_bits = table_log + 1 - weight;
                nb_per_rank[nb_bits] += 1;
            }
        }
        let mut val_per_rank = [0u16; 13];
        let mut min = 0u16;
        for nb_bits in (1..=table_log).rev() {
            val_per_rank[nb_bits] = min;
            min = min.wrapping_add(nb_per_rank[nb_bits]) >> 1;
        }
        for (symbol, &weight) in weights.iter().enumerate() {
            if weight == 0 {
                continue;
            }
            let nb_bits = table_log + 1 - weight;
            let value = val_per_rank[nb_bits];
            val_per_rank[nb_bits] += 1;
            table.codes[symbol] = (value as u32, nb_bits as u8);
            table.packed_codes[symbol] =
                super::huf_cstream::pack_huf_celt(value as u32, nb_bits as u8);
        }

        table
    }

    /// Upstream zstd-format packed code table for the hot encode loop.
    /// One `HUF_CElt` (`u64`) per symbol — see
    /// `huf_compress.c:208-221` for the layout.
    #[inline(always)]
    pub(crate) fn packed_codes(&self) -> &[u64] {
        &self.packed_codes
    }

    /// Active Huffman table-log (1..=12) — drives the `kUnroll`
    /// dispatch in `encode4x_reference`.
    #[inline(always)]
    pub(crate) fn table_log(&self) -> u32 {
        self.table_log
    }

    pub fn can_encode(&self, other: &Self) -> Option<usize> {
        if other.codes.len() > self.codes.len() {
            return None;
        }
        let mut sum = 0;
        for ((_, other_num_bits), (_, self_num_bits)) in other.codes.iter().zip(self.codes.iter()) {
            if *other_num_bits != 0 && *self_num_bits == 0 {
                return None;
            }
            sum += other_num_bits.abs_diff(*self_num_bits) as usize;
        }
        Some(sum)
    }

    pub(crate) fn num_bits_for_symbol(&self, symbol: u8) -> Option<u8> {
        self.codes
            .get(symbol as usize)
            .and_then(|&(_, bits)| if bits > 0 { Some(bits) } else { None })
    }
}

/// Cheap analytic estimate of the serialized Huffman weight-description
/// size in bytes — `None` when neither the FSE nor the raw representation
/// would be expressible.
///
/// Why: the previous `HuffmanTable::build_from_counts` search loop called
/// [`HuffmanTable::try_table_description_size`] per candidate, which runs
/// a full FSE-encode of the weight stream against a freshly-built FSE
/// table just to count bytes. For a 7-iteration `min_table_log..=11`
/// search that is 7× FSE encode + 7× FSE table build per block — ~31 %
/// inclusive on the 4 KiB profile (#167). This proxy reproduces the
/// upstream zstd `HUF_writeCTable_wksp` decision (FSE vs raw nibble) without
/// touching the FSE encoder.
///
/// Algorithm — both representations mirror the writer code in this file
/// (`HuffmanEncoder::encode_weight_description` / `write_raw_weight_description`):
///
/// 1. **Raw nibble.** Exact size = `weights.len().div_ceil(2) + 1`
///    (one length byte + packed nibbles). Representable when
///    `weights.len() <= 128`.
/// 2. **FSE.** Estimate the compressed payload via an integer entropy
///    bound over the 13-bin weight histogram: every weight `w` with
///    count `c` contributes `c * ceil_log2(total / c)` bits (uniform-prior
///    upper bound, no probability quantization). Add `FSE_HEADER_OVERHEAD`
///    bytes (4 bits `acc_log` + per-symbol probability stream + length
///    byte). Representable when the total serialized size is `<= 128`
///    bytes — the underlying writer (`encode_weight_description`)
///    rejects FSE payloads of `>= 128` bytes, so `payload_len + 1`
///    length prefix tops out at exactly 128.
///
/// `n == 0` returns `None`. The raw nibble path could technically
/// serialize an empty weight slice (just the length byte), but
/// production callers never hand `n == 0` here: `build_from_counts`
/// short-circuits the search loop when `symbol_cardinality <= 1`
/// (`HuffmanTable::build_from_counts` early return) and otherwise
/// `weights.len()` is `max_symbol + 1 >= 2`. Returning `None` keeps
/// the contract symmetric ("not representable" = "skip this
/// candidate") without an empty-slice special case.
///
/// Upstream zstd picks the smaller of FSE / raw when both are representable.
///
/// **Tolerance note:** the FSE entropy bound is generous — it never
/// undershoots a perfectly-tuned FSE encoder. In practice that means
/// the proxy may pick a slightly higher `table_log` in edge cases
/// where the real FSE description would have been a byte or two
/// smaller. Validated empirically against `try_table_description_size`
/// across the `compare_ffi` REPORT sweep (small-1k-random,
/// small-10k-random, small-4k-log-lines, low-entropy-1m,
/// high-entropy-1m, decodecorpus-z000033, large-log-stream × every
/// supported level): selection identical, ratio preserved.
fn cheap_desc_size_proxy(weights: &[u8]) -> Option<usize> {
    let n = weights.len();
    if n == 0 {
        return None;
    }
    let raw_ok = n <= 128;
    let raw_size = n.div_ceil(2) + 1;

    let mut hist = [0u32; 13];
    for &w in weights {
        debug_assert!(
            (w as usize) < hist.len(),
            "huffman weights are bounded to 0..12 by `build_limited_weights`"
        );
        hist[w as usize] += 1;
    }
    let total = n as u32;
    let mut bits: u64 = 0;
    for &c in &hist {
        if c == 0 {
            continue;
        }
        // `ceil_log2(ceil(total / c))` via integer formula. Use ceiling
        // division for `total / c` first — otherwise a fractional
        // ratio like `total=10, c=4 → 2.5` would truncate to `2`, the
        // `ceil_log2` would emit `1` bit, and the proxy would
        // *under-shoot* the real entropy bound (≥ 2 bits per symbol).
        // For `c == total` the ceiling ratio is `1` and the bound
        // collapses to `0`; clamp to `1` so a single-symbol weight
        // stream still gets one bit per symbol (matches FSE's minimum
        // encode width for present symbols).
        let ratio = total.div_ceil(c);
        let bits_per_symbol = if ratio <= 1 {
            1
        } else {
            32 - (ratio - 1).leading_zeros()
        };
        bits += (c as u64) * (bits_per_symbol as u64);
    }
    let fse_payload_bytes = bits.div_ceil(8) as usize;
    // FSE description overhead seen in `encode_weight_description`:
    // 4 bits `acc_log` + the `write_table` probability stream (~5 B for
    // a 13-symbol alphabet) + a 1-byte length prefix. 8 B is an
    // empirically-derived upper bound for our `acc_log = 6` weight tables.
    const FSE_HEADER_OVERHEAD_BYTES: usize = 8;
    let fse_size = fse_payload_bytes + FSE_HEADER_OVERHEAD_BYTES;
    // Upstream zstd `encode_weight_description` rejects only `encoded.len() >= 128`,
    // so `encoded.len() == 127` is the largest accepted FSE-encoded payload
    // and the total serialized description (`encoded.len() + 1` length-byte
    // prefix) is exactly 128 B in that boundary case. `fse_size` here is
    // the TOTAL including the length byte — accept `<= 128`, not `<= 127`,
    // otherwise the proxy would skip a valid candidate at the boundary.
    let fse_ok = fse_size <= 128;

    match (fse_ok, raw_ok) {
        (true, true) => Some(fse_size.min(raw_size)),
        (true, false) => Some(fse_size),
        (false, true) => Some(raw_size),
        (false, false) => None,
    }
}

fn huffman_weight_sum_is_power_of_two(weights: &[usize]) -> bool {
    let sum = weights
        .iter()
        .copied()
        .filter(|&weight| weight > 0)
        .map(|weight| 1usize << (weight - 1))
        .sum::<usize>();
    sum.is_power_of_two()
}

#[derive(Clone)]
struct HuffNode {
    count: usize,
    symbol: usize,
    parent: Option<usize>,
    nb_bits: usize,
}

/// Build the count-sorted Huffman leaves with their natural (unlimited) code
/// lengths in `nb_bits`. The tree shape is independent of any maximum-length
/// limit, so this is computed once per block and shared across every
/// candidate `table_log` instead of being rebuilt per candidate. The
/// returned leaves are sorted by (count desc, symbol asc); for <= 1 distinct
/// symbol the (0 or 1) leaves are returned with `nb_bits == 0` and the caller
/// assigns the trivial weight.
fn build_huffman_leaf_depths(counts: &[usize]) -> Vec<HuffNode> {
    let mut nodes = Vec::new();
    build_huffman_leaf_depths_into(counts, &mut nodes);
    nodes
}

/// [`build_huffman_leaf_depths`] filling a caller-owned buffer, so a caller
/// that builds a tree per block reuses one allocation instead of taking a fresh
/// one every time.
fn build_huffman_leaf_depths_into(counts: &[usize], nodes: &mut Vec<HuffNode>) {
    let leaf_count = counts.iter().filter(|&&count| count > 0).count();
    // Pre-size to the final node count (`2 * leaf_count - 1`) so the tree
    // build's resize never reallocates.
    nodes.clear();
    nodes.reserve((2 * leaf_count).max(1));

    if leaf_count == 0 {
        return;
    }
    if leaf_count == 1 {
        let (symbol, &count) = counts.iter().enumerate().find(|&(_, &c)| c > 0).unwrap();
        nodes.push(HuffNode {
            count,
            symbol,
            parent: None,
            nb_bits: 0,
        });
        return;
    }

    // Bucketed sort (upstream zstd `HUF_sort`, huf_compress.c): an O(n)
    // counting pass bucketed by the count magnitude. Counts below
    // `DISTINCT_CUTOFF` each get a dedicated bucket, so they land strictly
    // count-descending / symbol-ascending with no per-bucket sort; counts at
    // or above it share log2 buckets that ARE sorted. Sorting those log2
    // buckets by `(count desc, symbol asc)` reproduces the previous
    // comparison-sort order byte-for-byte while replacing the O(n log n) sort
    // with the bucket pass plus a handful of tiny bucket sorts — the Huffman
    // leaf sort was a measured ~7% of the per-frame entropy-build time.
    const BUCKETS: usize = 192;
    const LOG_BUCKETS_BEGIN: usize = 158; // (BUCKETS - 1) - 32 - 1
    const DISTINCT_CUTOFF: usize = 165; // LOG_BUCKETS_BEGIN + highbit32(158)
    let bucket_of = |count: usize| -> usize {
        if count < DISTINCT_CUTOFF {
            count
        } else {
            // `highest_bit_set(count) - 1` == upstream `ZSTD_highbit32`.
            (highest_bit_set(count) - 1) + LOG_BUCKETS_BEGIN
        }
    };
    let mut bucket_size = [0u32; BUCKETS];
    for &count in counts {
        if count > 0 {
            bucket_size[bucket_of(count)] += 1;
        }
    }
    // Output start index per bucket: higher buckets (higher counts) come
    // first, so the leaves end up count-descending.
    let mut start = [0u32; BUCKETS];
    let mut acc = 0u32;
    for bucket in (0..BUCKETS).rev() {
        start[bucket] = acc;
        acc += bucket_size[bucket];
    }
    let mut cursor = start;
    nodes.resize(
        leaf_count,
        HuffNode {
            count: 0,
            symbol: 0,
            parent: None,
            nb_bits: 0,
        },
    );
    for (symbol, &count) in counts.iter().enumerate() {
        if count == 0 {
            continue;
        }
        let bucket = bucket_of(count);
        let pos = cursor[bucket] as usize;
        cursor[bucket] += 1;
        nodes[pos] = HuffNode {
            count,
            symbol,
            parent: None,
            nb_bits: 0,
        };
    }
    // Only the log2 buckets mix distinct counts; sort them stably by
    // `(count desc, symbol asc)` so the order matches the old full sort.
    for bucket in DISTINCT_CUTOFF..BUCKETS {
        let lo = start[bucket] as usize;
        let hi = cursor[bucket] as usize;
        if hi - lo > 1 {
            nodes[lo..hi].sort_by(|left, right| match right.count.cmp(&left.count) {
                Ordering::Equal => left.symbol.cmp(&right.symbol),
                other => other,
            });
        }
    }

    nodes.resize(
        2 * leaf_count - 1,
        HuffNode {
            count: usize::MAX,
            symbol: usize::MAX,
            parent: None,
            nb_bits: 0,
        },
    );

    let mut low_s = leaf_count as isize - 1;
    let mut low_n = leaf_count;
    let node_root = leaf_count + (leaf_count - 1) - 1;
    let mut node_nb = leaf_count;

    // Plain `+`: node counts are symbol frequencies whose tree-wide sum is the
    // block's symbol count (<= MAX_BLOCK_SIZE), so a merged parent count cannot
    // overflow usize. Saturation would only mask a corrupt frequency table.
    nodes[node_nb].count = nodes[low_s as usize].count + nodes[(low_s - 1) as usize].count;
    nodes[node_nb].symbol = nodes[(low_s - 1) as usize]
        .symbol
        .min(nodes[low_s as usize].symbol);
    nodes[low_s as usize].parent = Some(node_nb);
    nodes[(low_s - 1) as usize].parent = Some(node_nb);
    node_nb += 1;
    low_s -= 2;

    while node_nb <= node_root {
        let first = {
            let leaf_count = if low_s >= 0 {
                nodes[low_s as usize].count
            } else {
                usize::MAX
            };
            let node_count = nodes[low_n].count;
            if leaf_count < node_count {
                let idx = low_s as usize;
                low_s -= 1;
                idx
            } else {
                let idx = low_n;
                low_n += 1;
                idx
            }
        };
        let second = {
            let leaf_count = if low_s >= 0 {
                nodes[low_s as usize].count
            } else {
                usize::MAX
            };
            let node_count = nodes[low_n].count;
            if leaf_count < node_count {
                let idx = low_s as usize;
                low_s -= 1;
                idx
            } else {
                let idx = low_n;
                low_n += 1;
                idx
            }
        };
        // Plain `+`: see the leaf-merge above — counts sum to <= MAX_BLOCK_SIZE.
        nodes[node_nb].count = nodes[first].count + nodes[second].count;
        nodes[node_nb].symbol = nodes[first].symbol.min(nodes[second].symbol);
        nodes[first].parent = Some(node_nb);
        nodes[second].parent = Some(node_nb);
        node_nb += 1;
    }

    for leaf_idx in 0..leaf_count {
        let mut depth = 0usize;
        let mut parent = nodes[leaf_idx].parent;
        while let Some(parent_idx) = parent {
            depth += 1;
            parent = nodes[parent_idx].parent;
        }
        nodes[leaf_idx].nb_bits = depth;
    }

    // The leaves keep their (count desc, symbol asc) order: the tree build
    // only writes `parent` / `nb_bits` on them and never reorders them or
    // changes their `count` / `symbol`. Return just the leaves (with depths).
    nodes.truncate(leaf_count);
}

/// Limit the natural Huffman depths in `leaves` (from
/// [`build_huffman_leaf_depths`]) to `max_nb_bits` and project them onto a
/// per-symbol weight table of length `counts_len`. Returns `None` when the
/// height limiting cannot reach `max_nb_bits` (the caller then falls back to
/// the distributed-weight construction). `leaves` must be sorted by count
/// descending, which `enforce_max_height` relies on.
/// Height-limit `leaves` to `max_nb_bits` and project onto a per-symbol weight
/// table, writing into `out` (cleared + sized to `counts_len`). `work` is a
/// caller-owned scratch buffer for the height-limiting pass; both buffers are
/// reused across the table-log candidate search so the per-candidate
/// `leaves.to_vec()` + fresh weight `Vec` allocations are gone. Returns `false`
/// when the height limiting cannot reach `max_nb_bits` (caller falls back).
fn limited_weights_into(
    leaves: &[HuffNode],
    counts_len: usize,
    max_nb_bits: usize,
    work: &mut Vec<HuffNode>,
    out: &mut Vec<usize>,
) -> bool {
    out.clear();
    out.resize(counts_len, 0);
    if leaves.len() <= 1 {
        if let Some(leaf) = leaves.first() {
            out[leaf.symbol] = 1;
        }
        return true;
    }

    work.clear();
    work.extend_from_slice(leaves);
    enforce_max_height(work, max_nb_bits);
    // The height limiter restores a full, canonical code in one pass (matching
    // upstream `HUF_setMaxHeight`): the Kraft sum `Σ 2^(max_nb_bits - nb_bits)`
    // lands exactly on `2^max_nb_bits`. A degenerate distribution can stop the
    // fill short (our node buffer is sized to the leaves, not over-sized like
    // upstream), leaving the code under- or over-full, which projects to a
    // non-power-of-two weight sum the table builder rejects; detect that here
    // and fall back to the distributed-weight construction.
    if work.iter().any(|leaf| leaf.nb_bits > max_nb_bits) {
        return false;
    }
    let kraft_sum = work
        .iter()
        .map(|leaf| 1usize << (max_nb_bits - leaf.nb_bits))
        .sum::<usize>();
    if kraft_sum != 1usize << max_nb_bits {
        return false;
    }

    for leaf in work.iter() {
        out[leaf.symbol] = max_nb_bits - leaf.nb_bits + 1;
    }
    true
}

/// Upstream HUF cheap/fast-path tableLog (`HUF_optimalTableLog`,
/// huf_compress.c:1286) for strategies below btultra, where the optimal-depth
/// probe is gated off: the single-shot
/// `FSE_optimalTableLog_internal(HUF_TABLELOG_DEFAULT = 11, srcSize, maxSV, minus = 1)`.
/// Degenerate `srcSize <= 1` (RLE-shaped, where `ilog2(srcSize - 1)` is undefined)
/// falls back to the natural-height cap of 11.
fn cheap_huf_table_log(counts: &[usize]) -> usize {
    let total: usize = counts.iter().sum();
    if total <= 1 {
        return 11;
    }
    let max_symbol = counts.iter().rposition(|&c| c > 0).unwrap_or(0);
    crate::fse::fse_encoder::optimal_table_log(11, total, max_symbol, 1) as usize
}

fn build_limited_weights(counts: &[usize], max_nb_bits: usize) -> Vec<usize> {
    let mut scratch = WeightScratch::default();
    build_limited_weights_into(counts, max_nb_bits, &mut scratch);
    core::mem::take(&mut scratch.weights)
}

/// The three buffers a weight build needs, owned by the caller so a compressor
/// that builds a table per block — and, where the block splitter probes, per
/// split candidate — reuses them instead of taking three fresh allocations
/// every time. The table-log SEARCH path already reused its scratch; this is
/// the same for the cheap single-build path.
#[derive(Default)]
pub(crate) struct WeightScratch {
    leaves: Vec<HuffNode>,
    work: Vec<HuffNode>,
    weights: Vec<usize>,
    /// The last table the caller built and threw away, handed back so the next
    /// build fills its buffers instead of taking new ones.
    spare_table: Option<HuffmanTable>,
}

impl WeightScratch {
    /// Heap bytes this scratch keeps between blocks and frames. It exists to
    /// hold its buffers rather than take them again, so a compressor asked for
    /// its footprint — including through the C API's `ZSTD_sizeof_CCtx` — has
    /// to be told about them.
    ///
    /// A parked table contributes its own heap bytes only: the table object
    /// sits inline here, as the three buffers' headers do, and none of them
    /// are an allocation of this scratch's making.
    pub(crate) fn heap_size(&self) -> usize {
        self.leaves.capacity() * core::mem::size_of::<HuffNode>()
            + self.work.capacity() * core::mem::size_of::<HuffNode>()
            + self.weights.capacity() * core::mem::size_of::<usize>()
            + self.spare_table.as_ref().map_or(0, HuffmanTable::heap_size)
    }

    /// Park a table the caller is done with, for the next build to fill.
    pub(crate) fn recycle(&mut self, table: HuffmanTable) {
        self.spare_table = Some(table);
    }
}

/// [`build_limited_weights`] filling caller-owned scratch. The weights land in
/// `scratch.weights`.
fn build_limited_weights_into(counts: &[usize], max_nb_bits: usize, scratch: &mut WeightScratch) {
    build_huffman_leaf_depths_into(counts, &mut scratch.leaves);
    if !limited_weights_into(
        &scratch.leaves,
        counts.len(),
        max_nb_bits,
        &mut scratch.work,
        &mut scratch.weights,
    ) {
        // The fallback builds its own; hand its result to the same slot so the
        // caller reads one place either way.
        scratch.weights = legacy_distributed_weights(counts);
    }
}

fn legacy_distributed_weights(counts: &[usize]) -> Vec<usize> {
    let zeros = counts.iter().filter(|x| **x == 0).count();
    let mut weights = distribute_weights(counts.len() - zeros);
    let limit = weights.len().ilog2() as usize + 2;
    redistribute_weights(&mut weights, limit);

    weights.reverse();
    let mut counts_sorted = counts.iter().enumerate().collect::<Vec<_>>();
    counts_sorted.sort_by_key(|(_, c1)| *c1);

    let mut weights_distributed = alloc::vec![0; counts.len()];
    for (idx, count) in counts_sorted {
        if *count == 0 {
            weights_distributed[idx] = 0;
        } else {
            weights_distributed[idx] = weights.pop().unwrap();
        }
    }
    weights_distributed
}

fn enforce_max_height(nodes: &mut [HuffNode], target_nb_bits: usize) {
    let Some(largest_bits) = nodes.iter().map(|node| node.nb_bits).max() else {
        return;
    };
    if largest_bits <= target_nb_bits {
        return;
    }

    // The shift is bounded by the input, not by anything checked here, so pin
    // the reasoning: a Huffman code of depth d needs at least Fib(d) symbols
    // counted, and a literals section is at most 128 KiB, which caps natural
    // depth around 25. `target_nb_bits` is at least 5, leaving a shift under 20
    // and comfortably inside a 32-bit `usize`. Worth asserting rather than
    // assuming: on a 32-bit target an over-large shift does not panic in
    // release, it silently masks to a smaller one and computes a wrong cost.
    debug_assert!(
        largest_bits - target_nb_bits < usize::BITS as usize,
        "code depth {largest_bits} over target {target_nb_bits} exceeds what a \
         usize shift can express; counts imply an impossibly large section",
    );
    let base_cost = 1usize << (largest_bits - target_nb_bits);
    let mut total_cost = 0isize;
    let mut n = nodes.len() - 1;
    while nodes[n].nb_bits > target_nb_bits {
        total_cost += (base_cost - (1usize << (largest_bits - nodes[n].nb_bits))) as isize;
        nodes[n].nb_bits = target_nb_bits;
        if n == 0 {
            break;
        }
        n -= 1;
    }
    while n > 0 && nodes[n].nb_bits == target_nb_bits {
        n -= 1;
    }
    total_cost >>= largest_bits - target_nb_bits;

    const NO_SYMBOL: usize = usize::MAX;
    // `rank_last` is indexed `0..=target_nb_bits + 1`. `target_nb_bits` is the
    // Huffman table log, capped at upstream zstd's `HUF_TABLELOG_MAX` (12), so a
    // fixed 14-slot stack array covers every valid call and avoids the
    // per-frame heap allocation this build performed for a ≤14-element scratch.
    debug_assert!(
        target_nb_bits + 2 <= 14,
        "target_nb_bits {target_nb_bits} exceeds HUF_TABLELOG_MAX scratch bound"
    );
    let mut rank_last = [NO_SYMBOL; 14];
    let mut current_nb_bits = target_nb_bits;
    for pos in (0..=n).rev() {
        if nodes[pos].nb_bits >= current_nb_bits {
            continue;
        }
        current_nb_bits = nodes[pos].nb_bits;
        rank_last[target_nb_bits - current_nb_bits] = pos;
    }

    while total_cost > 0 {
        let mut bits_to_decrease = (total_cost as usize).ilog2() as usize + 1;
        while bits_to_decrease > 1 {
            let high_pos = rank_last[bits_to_decrease];
            let low_pos = rank_last[bits_to_decrease - 1];
            if high_pos == NO_SYMBOL {
                bits_to_decrease -= 1;
                continue;
            }
            if low_pos == NO_SYMBOL {
                break;
            }
            if nodes[high_pos].count <= 2 * nodes[low_pos].count {
                break;
            }
            bits_to_decrease -= 1;
        }
        while bits_to_decrease <= target_nb_bits && rank_last[bits_to_decrease] == NO_SYMBOL {
            bits_to_decrease += 1;
        }
        if bits_to_decrease > target_nb_bits {
            return;
        }
        let pos = rank_last[bits_to_decrease];
        total_cost -= 1isize << (bits_to_decrease - 1);
        nodes[pos].nb_bits += 1;

        if rank_last[bits_to_decrease - 1] == NO_SYMBOL {
            rank_last[bits_to_decrease - 1] = pos;
        }
        if pos == 0 {
            rank_last[bits_to_decrease] = NO_SYMBOL;
        } else {
            let next = pos - 1;
            rank_last[bits_to_decrease] =
                if nodes[next].nb_bits == target_nb_bits - bits_to_decrease {
                    next
                } else {
                    NO_SYMBOL
                };
        }
    }

    // Cost correction can overshoot below zero. Add the weight back so the
    // final Kraft sum lands exactly on 2^target_nb_bits (a full, canonical
    // code); otherwise the weights are non-power-of-two and downstream table
    // construction rejects them. Only the smallest rank is adjusted to avoid
    // overshooting again: take the largest node from rank 0 (target_nb_bits)
    // and shorten it to rank 1 (target_nb_bits - 1). The highest-count symbol
    // (nodes[0]) keeps the shortest code, so its bit length stays strictly
    // below target_nb_bits while the tree is over-tall, which bounds the
    // `n` walk-down and guarantees this loop converges.
    while total_cost < 0 {
        if rank_last[1] == NO_SYMBOL {
            // No rank-1 symbol yet: create one from the largest rank-0 node.
            while n > 0 && nodes[n].nb_bits == target_nb_bits {
                n -= 1;
            }
            // Upstream relies on an over-sized node buffer here and reads into
            // its scratch tail; ours is sized exactly to the leaves. If there
            // is no real rank-0 node left to borrow, the distribution is too
            // degenerate to height-limit, so stop and let the caller fall back
            // to the distributed-weight construction.
            if nodes[n].nb_bits == target_nb_bits || n + 1 >= nodes.len() {
                break;
            }
            nodes[n + 1].nb_bits -= 1;
            rank_last[1] = n + 1;
            total_cost += 1;
            continue;
        }
        if rank_last[1] + 1 >= nodes.len() {
            break;
        }
        nodes[rank_last[1] + 1].nb_bits -= 1;
        rank_last[1] += 1;
        total_cost += 1;
    }
}

/// Assert that the provided value is greater than zero, and returns index of the first set bit
fn highest_bit_set(x: usize) -> usize {
    assert!(x > 0);
    usize::BITS as usize - x.leading_zeros() as usize
}

/// Distributes weights that add up to a clean power of two
fn distribute_weights(amount: usize) -> Vec<usize> {
    assert!(amount >= 2);
    assert!(amount <= 256);
    let mut weights = Vec::new();

    // This is the trivial power of two we always need
    weights.push(1);
    weights.push(1);

    // This is the weight we are adding right now
    let mut target_weight = 1;
    // Counts how many times we have added weights
    let mut weight_counter = 2;

    // We always add a power of 2 new weights so that the weights that we add equal
    // the weights are already in the vec if raised to the power of two.
    // This means we double the weights in the vec -> results in a new power of two
    //
    // Example: [1, 1]      -> [1,1,2]       (2^1 + 2^1 == 2^2)
    //
    // Example: [1, 1]      -> [1,1,1,1]     (2^1 + 2^1 == 2^1 + 2^1)
    //          [1,1,1,1]   -> [1,1,1,1,3]   (2^1 + 2^1 + 2^1 + 2^1 == 2^3)
    while weights.len() < amount {
        let mut add_new = 1 << (weight_counter - target_weight);
        let available_space = amount - weights.len();

        // If the amount of new weights needed to get to the next power of two would exceed amount
        // We instead add 1 of a bigger weight and start the cycle again
        if add_new > available_space {
            // TODO we could maybe instead do this until add_new <= available_space?
            //  target_weight += 1
            //  add_new /= 2
            target_weight = weight_counter;
            add_new = 1;
        }

        for _ in 0..add_new {
            weights.push(target_weight);
        }
        weight_counter += 1;
    }

    assert_eq!(amount, weights.len());

    weights
}

/// Sometimes distribute_weights generates weights that require too many bits to encode
/// This redistributes the weights to have less variance by raising the lower weights while still maintaining the
/// required attributes of the weight distribution
fn redistribute_weights(weights: &mut [usize], max_num_bits: usize) {
    let weight_sum_log = weights
        .iter()
        .copied()
        .map(|x| 1 << x)
        .sum::<usize>()
        .ilog2() as usize;

    // Nothing needs to be done, this is already fine
    if weight_sum_log < max_num_bits {
        return;
    }

    // We need to decrease the weight difference by the difference between weight_sum_log and max_num_bits
    let decrease_weights_by = weight_sum_log - max_num_bits + 1;

    // To do that we raise the lower weights up by that difference, recording how much weight we added in the process
    let mut added_weights = 0;
    for weight in weights.iter_mut() {
        if *weight < decrease_weights_by {
            for add in *weight..decrease_weights_by {
                added_weights += 1 << add;
            }
            *weight = decrease_weights_by;
        }
    }

    // Then we reduce weights until the added weights are equaled out
    while added_weights > 0 {
        // Find the highest weight that is still lower or equal to the added weight
        let mut current_idx = 0;
        let mut current_weight = 0;
        for (idx, weight) in weights.iter().copied().enumerate() {
            if 1 << (weight - 1) > added_weights {
                break;
            }
            if weight > current_weight {
                current_weight = weight;
                current_idx = idx;
            }
        }

        // Reduce that weight by 1
        added_weights -= 1 << (current_weight - 1);
        weights[current_idx] -= 1;
    }

    // At the end we normalize the weights so that they start at 1 again
    if weights[0] > 1 {
        let offset = weights[0] - 1;
        for weight in weights.iter_mut() {
            *weight -= offset;
        }
    }
}

/// White-box capture of the FSE-coded Huffman weight description our encoder
/// emits for `data` (a length byte followed by the FSE payload), plus the raw
/// per-symbol weights. Returns `(description, weights)`. The C-conformance
/// check that feeds this through `HUF_readStats` lives in the `ffi-bench`
/// crate; this side stays pure Rust.
#[cfg(feature = "bench-internals")]
pub(crate) fn huf_weight_description_for_test(data: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let table = HuffmanTable::build_from_data(data);
    let mut buf = [0u8; MAX_HUFFMAN_ALPHABET];
    let mut weights = table.weights_into(&mut buf).to_vec();
    weights.pop();
    let encoded = HuffmanEncoder::<Vec<u8>>::encode_weight_description(&weights)
        .expect("expected FSE weights");
    let mut description = Vec::with_capacity(encoded.len() + 1);
    description.push(encoded.len() as u8);
    description.extend_from_slice(&encoded);
    (description, weights)
}

/// White-box capture of the 4-stream Huffman payload (table description
/// followed by the coded streams) our encoder emits for `data`. The
/// C-conformance check that decodes it via `HUF_decompress4X_hufOnly_wksp`
/// lives in the `ffi-bench` crate.
#[cfg(feature = "bench-internals")]
pub(crate) fn huf_encode4x_for_test(data: &[u8]) -> Vec<u8> {
    let table = HuffmanTable::build_from_data(data);
    let mut encoded = Vec::new();
    {
        let mut writer = BitWriter::from(&mut encoded);
        let mut encoder = HuffmanEncoder::new(&table, &mut writer);
        encoder.encode4x(data, true);
        writer.flush();
    }
    encoded
}

#[cfg(test)]
mod tests;
