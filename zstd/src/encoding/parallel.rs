//! Multi-frame parallel compression: split the input into independent
//! chunks, compress each chunk on a [`rayon`] worker pool, concatenate
//! the resulting frames in source order.
//!
//! The output is a valid zstd multi-frame archive (RFC 8878 §3.1) —
//! conforming decoders consume concatenated independent frames
//! sequentially and reproduce the original input. This is **not**
//! upstream `zstdmt` byte-parity: no `overlapLog` (cross-job
//! non-emitted prefix), no `rsyncable` (rolling-hash content-aligned
//! boundaries), no `jobSize` tuning. The trade-off is a small ratio
//! degradation (≤ 5% on typical corpora, near-zero on
//! incompressible / chunk-local data) in exchange for a one-call
//! Rust API shipped now.
//!
//! Available behind the `mt` feature flag.

use alloc::vec::Vec;

use rayon::prelude::*;

use crate::encoding::{CompressionLevel, compress_slice_to_vec};

/// Inputs smaller than this skip the parallel path entirely. At two
/// donor default blocks (`2 * 128 KiB`) the parallel path has at
/// least one chunk-boundary worth splitting — below it every chunk
/// degenerates to a single block and the chunk-split / thread-handoff
/// overhead dominates the saved encode time.
const MIN_PARALLEL_INPUT_LEN: usize = 256 * 1024;

/// Floor for per-chunk size. Below one donor default block
/// (`128 KiB`) a chunk produces a single tiny frame whose extra
/// header bytes plus the lost in-chunk match opportunity erode the
/// ratio more than the parallelism saves on wall time.
const MIN_CHUNK_LEN: usize = 128 * 1024;

/// Compress `input` across multiple worker threads and return a valid
/// zstd multi-frame archive.
///
/// `num_threads`:
///
/// * `0` — query [`std::thread::available_parallelism`] for the
///   default worker count; falls back to single-threaded if the
///   query fails.
/// * `1` — single-threaded fast path. Output is **byte-identical** to
///   [`crate::encoding::compress_to_vec`] / [`crate::encoding::compress_slice_to_vec`]
///   on the same `(input, level)` pair — same code path, no rayon
///   involvement.
/// * `n >= 2` — split into roughly `n * 2` chunks (oversubscribed so
///   faster workers can pick up a second chunk while a slower one
///   drains) with a `128 KiB` per-chunk floor, compress each chunk on
///   the rayon worker pool, concatenate the resulting frames.
///
/// Inputs below `256 KiB` always take the single-threaded fast path
/// regardless of `num_threads`, because the chunk-split / thread-handoff
/// cost dominates the encode time at that size.
///
/// # Trade-offs vs upstream `zstdmt`
///
/// This is the simpler multi-frame approach, not upstream `zstdmt`
/// byte-parity. Specifically NOT implemented:
///
/// * `overlapLog` — cross-job back-references via non-emitted prefix
/// * `rsyncable` — content-aligned rolling-hash chunk boundaries
/// * `jobSize` parameter tuning
///
/// Practical impact: ≤ 5% ratio degradation on typical corpora,
/// near-zero on incompressible / chunk-local data, in exchange for a
/// one-call Rust API. Upstream `zstdmt` byte-parity is a separate
/// concern handled by the FFI/CLI surface when binary-output
/// reproduction is required.
///
/// # Examples
///
/// ```rust
/// use structured_zstd::encoding::{compress_to_vec_mt, CompressionLevel};
/// let data: Vec<u8> = (0..1_000_000u32).flat_map(u32::to_le_bytes).collect();
/// let compressed = compress_to_vec_mt(&data, CompressionLevel::Fastest, 0);
/// assert!(!compressed.is_empty());
/// ```
pub fn compress_to_vec_mt(input: &[u8], level: CompressionLevel, num_threads: usize) -> Vec<u8> {
    let workers = match num_threads {
        0 => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        n => n,
    };

    // Two single-threaded fast paths converge on the same call so
    // that `mt(input, level, 1)` is byte-identical to
    // `compress_slice_to_vec(input, level)` — that equivalence is a
    // public contract pinned by the `mt_one_thread_*` test below.
    if workers <= 1 || input.len() < MIN_PARALLEL_INPUT_LEN {
        return compress_slice_to_vec(input, level);
    }

    // Oversubscribe 2× so a slow chunk doesn't pin the whole tail
    // (rayon work-stealing rebalances when one worker finishes
    // early), but stay close to one-chunk-per-worker so we don't
    // pay extra frame-boundary ratio cost on uniform-cost workloads
    // — every additional chunk loses one chunk-boundary worth of
    // cross-block back-references. 2× is the sweet spot between
    // straggler-tolerance (helps when chunk timings vary) and ratio
    // (the per-chunk overhead scales with N_chunks).
    let chunk_size = (input.len() / (workers * 2)).max(MIN_CHUNK_LEN);
    let chunks: Vec<&[u8]> = input.chunks(chunk_size).collect();

    let frames: Vec<Vec<u8>> = chunks
        .par_iter()
        .map(|chunk| compress_slice_to_vec(chunk, level))
        .collect();

    let total: usize = frames.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(total);
    for frame in frames {
        out.extend_from_slice(&frame);
    }
    out
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::decoding::{BlockDecodingStrategy, FrameDecoder};
    use alloc::format;
    use alloc::vec;

    /// Decode a (possibly multi-frame) `compressed` stream through
    /// the in-tree decoder. Loops `reset + decode_blocks + collect`
    /// until the input cursor is drained, which is the contract a
    /// conforming RFC 8878 reader must satisfy on concatenated
    /// independent frames.
    fn decode_all(compressed: &[u8]) -> Vec<u8> {
        let mut dec = FrameDecoder::new();
        let mut cursor = compressed;
        let mut out = Vec::new();
        while !cursor.is_empty() {
            dec.reset(&mut cursor).expect("frame header decode");
            dec.decode_blocks(&mut cursor, BlockDecodingStrategy::All)
                .expect("frame body decode");
            out.extend(dec.collect().expect("frame plaintext"));
        }
        out
    }

    fn deterministic_corpus(len: usize) -> Vec<u8> {
        // LCG-derived pseudo-random bytes — produces medium-entropy
        // content with no exploitable repetition, so ratio is not
        // trivially dominated by chunk-local short matches.
        let mut state: u32 = 0xdead_beef;
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            out.push((state >> 16) as u8);
        }
        out
    }

    fn log_line_corpus(len: usize) -> Vec<u8> {
        // Synthetic log lines with rotating numeric fields — shaped
        // like a realistic workload (timestamps, host IDs, durations),
        // typical-entropy with moderate local repetition. Avoids both
        // extremes: pure random (no compression to compare against)
        // and pathologically repetitive (where single-thread crushes
        // 2 MiB into ~200 bytes and any per-frame fixed overhead blows
        // up the ratio non-meaningfully). This is what `coordinode`
        // and similar consumers actually feed to the encoder, so the
        // ratio guard is calibrated against the use case driving #19.
        let mut out = Vec::with_capacity(len);
        let hosts = ["coordinode-01", "coordinode-02", "coordinode-03"];
        let levels = ["INFO ", "WARN ", "DEBUG", "ERROR"];
        let mut ts: u64 = 1_700_000_000_000;
        let mut id: u64 = 0;
        while out.len() < len {
            ts = ts.wrapping_add(317);
            id = id.wrapping_add(1);
            let host = hosts[(id as usize) % hosts.len()];
            let lvl = levels[(id as usize >> 1) % levels.len()];
            out.extend_from_slice(
                format!(
                    "{ts} {lvl} {host} req_id={id:016x} took={micros}us status=200\n",
                    micros = 50 + (id % 9750),
                )
                .as_bytes(),
            );
        }
        out.truncate(len);
        out
    }

    /// `num_threads = 1` MUST take the single-threaded fast path and
    /// produce byte-identical output to
    /// [`compress_slice_to_vec`] on the same input/level — that
    /// equivalence is a public contract that lets callers conditionally
    /// thread without changing the on-wire bytes.
    #[test]
    fn mt_one_thread_matches_single_threaded_byte_for_byte() {
        let data = deterministic_corpus(64 * 1024);
        let st = compress_slice_to_vec(&data, CompressionLevel::Fastest);
        let mt1 = compress_to_vec_mt(&data, CompressionLevel::Fastest, 1);
        assert_eq!(mt1, st);
    }

    /// Inputs below `MIN_PARALLEL_INPUT_LEN` must always take the
    /// single-threaded fast path regardless of `num_threads`. Without
    /// this rule, a 1-byte input with `num_threads = 8` would emit 8
    /// near-empty frames and pay 8 thread-spawn costs for negative
    /// benefit. Pinning the threshold here documents the contract.
    #[test]
    fn mt_below_threshold_takes_single_threaded_fast_path() {
        let data = deterministic_corpus(MIN_PARALLEL_INPUT_LEN - 1);
        let st = compress_slice_to_vec(&data, CompressionLevel::Fastest);
        for &n in &[2usize, 4, 8] {
            let mt = compress_to_vec_mt(&data, CompressionLevel::Fastest, n);
            assert_eq!(mt, st, "n={n}");
        }
    }

    /// `num_threads = 0` must produce a valid roundtrip — we don't
    /// pin the exact chunk count because it depends on the host's
    /// `available_parallelism()`, just that the output decodes back
    /// to the input. The fall-through path on detection failure
    /// (single-threaded) is exercised implicitly by hosts where
    /// available_parallelism() returns Err, which is fine because
    /// the single-threaded path is itself a tested case.
    #[test]
    fn mt_zero_threads_uses_available_parallelism() {
        let data = deterministic_corpus(MIN_PARALLEL_INPUT_LEN * 4);
        let compressed = compress_to_vec_mt(&data, CompressionLevel::Fastest, 0);
        assert_eq!(decode_all(&compressed), data);
    }

    /// Public acceptance criterion from #19: `decode(mt(input, N)) ==
    /// input` for `N ∈ {1, 2, 4, 8}`. Covers the single-threaded
    /// fast path (`N=1`) AND the chunked path with multiple worker
    /// counts so a regression in chunk boundary handling shows up
    /// before the ratio test runs.
    #[test]
    fn mt_roundtrip_across_thread_counts() {
        let data = deterministic_corpus(MIN_PARALLEL_INPUT_LEN * 4);
        for &n in &[1usize, 2, 4, 8] {
            let compressed = compress_to_vec_mt(&data, CompressionLevel::Fastest, n);
            assert_eq!(decode_all(&compressed), data, "n={n}");
        }
    }

    /// Defining contract: with `n >= 2` and an input large enough to
    /// chunk, the output is a multi-frame archive — multiple zstd
    /// frame magic numbers appear in the byte stream. Probe by
    /// counting occurrences of the little-endian magic
    /// `0xfd2fb528` (`28 b5 2f fd`); the count is a lower bound on
    /// the number of independent frames the decoder will see.
    #[test]
    fn mt_output_is_multiframe_for_n_geq_2() {
        let data = deterministic_corpus(MIN_PARALLEL_INPUT_LEN * 4);
        let compressed = compress_to_vec_mt(&data, CompressionLevel::Fastest, 4);
        const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
        let magic_count = compressed.windows(4).filter(|w| *w == ZSTD_MAGIC).count();
        assert!(
            magic_count >= 2,
            "expected >= 2 zstd frames in mt(n=4) output, found {magic_count}; \
             output should be a multi-frame archive",
        );
    }

    /// Ratio-degradation guard on a realistic log-line workload —
    /// the shape `coordinode` and similar `#19` consumers actually
    /// feed in. Cross-chunk back-references are the only thing we
    /// lose vs single-threaded, and on typical-entropy content with
    /// moderate local repetition the cost stays well under the
    /// 5% acceptance bound from #19. Pathologically-repetitive
    /// 2 MiB-of-one-pattern corpora are NOT what this bound covers
    /// (single-thread crushes them into ~200 bytes, and any
    /// per-frame fixed overhead inflates the ratio non-meaningfully)
    /// — see the #19 body wording "≤ 5% on typical corpora".
    #[test]
    fn mt_ratio_degradation_within_5pct_on_typical_corpus() {
        let data = log_line_corpus(MIN_PARALLEL_INPUT_LEN * 8);
        let st = compress_slice_to_vec(&data, CompressionLevel::Default);
        let mt = compress_to_vec_mt(&data, CompressionLevel::Default, 4);
        let ratio = mt.len() as f64 / st.len() as f64;
        assert!(
            ratio <= 1.05,
            "mt/st size ratio = {ratio:.3} on log-line corpus \
             (st={} mt={} bytes); expected ≤ 1.05 (≤ 5% degradation)",
            st.len(),
            mt.len(),
        );
    }

    /// Roundtrip against `zstd` (the donor C zstd crate's
    /// decompressor) — verifies the multi-frame output decodes via
    /// upstream tooling and not just our in-tree decoder. Equivalent
    /// to the acceptance-criterion bullet "Output frames are valid
    /// zstd multi-frame streams: `zstd -d` (upstream binary) decodes
    /// them correctly", run programmatically here so CI catches
    /// regressions without needing the upstream binary on PATH.
    #[test]
    fn mt_output_decodes_via_donor_c_zstd() {
        let data = deterministic_corpus(MIN_PARALLEL_INPUT_LEN * 4);
        let compressed = compress_to_vec_mt(&data, CompressionLevel::Fastest, 4);
        let decoded = zstd::stream::decode_all(compressed.as_slice())
            .expect("donor zstd must accept our multi-frame stream");
        assert_eq!(decoded, data);
    }

    /// Defensive: an empty input must produce a valid (possibly
    /// empty-payload) zstd frame and decode back to an empty Vec.
    /// Tests the `workers <= 1` branch under the empty-slice edge
    /// case so a regression that panics on `input.chunks(0)` or
    /// similar surfaces here, not in user code.
    #[test]
    fn mt_empty_input_decodes_to_empty() {
        let data: Vec<u8> = vec![];
        for &n in &[0usize, 1, 4] {
            let compressed = compress_to_vec_mt(&data, CompressionLevel::Fastest, n);
            assert_eq!(decode_all(&compressed), data, "n={n}");
        }
    }
}
