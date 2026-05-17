//! Sequence-stream comparator: structured-zstd (pure Rust) vs zstd (C FFI).
//!
//! For a fixed `(input, level)` pair, capture the encoder sequence stream
//! from both sides and diff `(literal_length, offset, match_length)`
//! triples to triage residual compression-ratio gaps.
//!
//! Pipeline:
//! 1. Rust side — drive the production [`FrameCompressor`] via
//!    [`structured_zstd::encoding::sequence_capture::compress_and_collect_sequences`].
//!    Returns one `CapturedRawSequence { block_idx, seq_in_block, ll, of, ml }`
//!    per `Sequence::Triple` emitted by the matcher.
//! 2. FFI side — `ZSTD_generateSequences` against the same `level`. The
//!    donor emits a per-block stream where every block ends with a
//!    dummy delimiter `(of=0, ml=0, ll=trailing_literals)`. Delimiters
//!    are dropped here and used solely to tag block indices on the
//!    surviving sequences.
//! 3. Alignment — both streams are in input-order. Walk in lockstep by
//!    cumulative consumed bytes (`Σ ll + ml`). At each step the side
//!    with the smaller cumulative position advances; equal positions
//!    are compared directly.
//! 4. Output — per-fixture summary + the first `MAX_DIVERGENCE_ROWS`
//!    diverging rows in a plain-text table.
//!
//! Fixtures (Lane D, `7-tooling-seq-cmp`, first iteration):
//! - `decodecorpus_files/z000033` — pre-existing corpus fixture that
//!   surfaces the −7% size delta on `level_3_dfast` vs C zstd, i.e.
//!   the canonical signal this tool exists to triage.
//! - Synthesized low-entropy log — 16 KiB of repeating log-line shapes
//!   with rotating numeric fields, exercising medium-repetition input
//!   that real workloads produce.
//!
//! `harness = false` + this file owns `fn main()` — criterion is not
//! used because timing is irrelevant here; this is a diagnostic
//! one-shot, not a regression bench.

// `support` is shared with `compare_ffi.rs` / `compare_ffi_memory.rs`.
// This bench doesn't use any of its helpers but the module is still
// pulled in by Cargo because `bench` targets share `benches/`. The
// `#[allow(dead_code)]` suppresses warnings about unused items in
// `support`.
#[allow(dead_code)]
mod support;

use std::fs;
use std::path::Path;

use structured_zstd::encoding::CompressionLevel;
use structured_zstd::encoding::sequence_capture::{
    CapturedRawSequence, compress_and_collect_sequences,
};

/// Compression level under audit. Level 3 / Dfast is the
/// user-visible `Default` preset and the focus of the Lane A
/// `7-compress-default` sub-phase that this tool gates.
const TARGET_LEVEL: i32 = 3;

/// Cap on diverging-row output per fixture. Beyond this, only the
/// summary counts are printed — first-N is sufficient for triage,
/// the full diff is recoverable by re-running with the cap raised.
const MAX_DIVERGENCE_ROWS: usize = 30;

/// One sequence captured from the donor (`ZSTD_generateSequences`).
/// Block delimiters (`of=0 ml=0`) are filtered out before construction.
#[derive(Clone, Copy, Debug)]
struct FfiSeq {
    block_idx: u32,
    seq_in_block: u32,
    ll: u32,
    of: u32,
    ml: u32,
}

/// Result of comparing one position in the joined stream.
#[derive(Clone, Copy, Debug)]
enum DiffRow {
    Equal,
    Differ {
        rust: CapturedRawSequence,
        ffi: FfiSeq,
    },
    RustOnly(CapturedRawSequence),
    FfiOnly(FfiSeq),
}

fn main() {
    let fixtures = collect_fixtures();
    println!(
        "=== compare_ffi_sequences (level={}, Rust vs C FFI) ===",
        TARGET_LEVEL
    );
    println!();
    for (name, bytes) in fixtures {
        run_one(&name, &bytes);
        println!();
    }
}

/// Build the fixture set. Returns `(name, bytes)` pairs ready to feed
/// into both encoders. The lookup is intentionally permissive — if the
/// disk fixture is missing (e.g. `decodecorpus_files/` excluded from a
/// downstream packaging), the synthetic fixture still runs so the
/// bench is never a hard failure on a fresh checkout.
fn collect_fixtures() -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();

    let corpus_path = Path::new("decodecorpus_files/z000033");
    let workspace_corpus_path = Path::new("zstd/decodecorpus_files/z000033");
    let corpus = if corpus_path.exists() {
        Some(corpus_path)
    } else if workspace_corpus_path.exists() {
        Some(workspace_corpus_path)
    } else {
        None
    };
    if let Some(p) = corpus {
        match fs::read(p) {
            Ok(bytes) => out.push((format!("z000033 ({} bytes)", bytes.len()), bytes)),
            Err(e) => eprintln!("warn: skipping {} — {}", p.display(), e),
        }
    } else {
        eprintln!(
            "warn: decodecorpus z000033 fixture not found at {} or {} — skipping",
            corpus_path.display(),
            workspace_corpus_path.display()
        );
    }

    let log = build_low_entropy_log(16 * 1024);
    out.push((format!("low-entropy log ({} bytes)", log.len()), log));

    out
}

/// Synthesize repeating log-line shapes for `byte_budget` bytes.
/// The pattern rotates a numeric field every line so the matcher
/// can't lock onto a single long repcode and instead emits a stream
/// of medium matches with the rotating fragment as literals — the
/// shape Rust↔FFI tends to diverge on.
fn build_low_entropy_log(byte_budget: usize) -> Vec<u8> {
    let prefix = b"2026-05-17T12:00:00 INFO  request_id=";
    let suffix = b" route=/api/v1/items method=GET status=200 elapsed_ms=";
    let mut out = Vec::with_capacity(byte_budget);
    let mut counter: u32 = 0;
    while out.len() < byte_budget {
        out.extend_from_slice(prefix);
        // 8-hex-digit rotating request_id keeps the line shape stable
        // while preventing trivial RLE / long-repcode collapse.
        let hex = format!("{:08x}", counter);
        out.extend_from_slice(hex.as_bytes());
        out.extend_from_slice(suffix);
        let elapsed = format!("{}", counter % 1000);
        out.extend_from_slice(elapsed.as_bytes());
        out.push(b'\n');
        counter = counter.wrapping_add(1);
    }
    out.truncate(byte_budget);
    out
}

fn run_one(name: &str, input: &[u8]) {
    let rust_seqs = compress_and_collect_sequences(input, CompressionLevel::Level(TARGET_LEVEL));
    let ffi_seqs = ffi_generate_sequences(input, TARGET_LEVEL);

    println!("--- fixture: {name} ---");
    println!(
        "  rust sequences: {} (across {} block(s))",
        rust_seqs.len(),
        rust_seqs.last().map(|s| s.block_idx + 1).unwrap_or(0),
    );
    println!(
        "  ffi  sequences: {} (across {} block(s))",
        ffi_seqs.len(),
        ffi_seqs.last().map(|s| s.block_idx + 1).unwrap_or(0),
    );

    let rows = align_and_diff(&rust_seqs, &ffi_seqs);
    let mut equal = 0usize;
    let mut differ = 0usize;
    let mut rust_only = 0usize;
    let mut ffi_only = 0usize;
    for r in &rows {
        match r {
            DiffRow::Equal => equal += 1,
            DiffRow::Differ { .. } => differ += 1,
            DiffRow::RustOnly(_) => rust_only += 1,
            DiffRow::FfiOnly(_) => ffi_only += 1,
        }
    }
    let total = rows.len().max(1);
    println!(
        "  alignment: equal={equal} ({:.1}%) differ={differ} rust_only={rust_only} ffi_only={ffi_only}",
        equal as f64 * 100.0 / total as f64,
    );

    let mut printed = 0usize;
    let mut header_printed = false;
    for (idx, r) in rows.iter().enumerate() {
        if matches!(r, DiffRow::Equal) {
            continue;
        }
        if printed >= MAX_DIVERGENCE_ROWS {
            println!(
                "  ... (omitted {} more diverging rows; raise MAX_DIVERGENCE_ROWS to see them)",
                differ + rust_only + ffi_only - printed,
            );
            break;
        }
        if !header_printed {
            println!();
            println!(
                "  {:>5} | {:^28} | {:^28} | verdict",
                "idx", "rust (blk:idx ll/of/ml)", "ffi  (blk:idx ll/of/ml)"
            );
            println!("  {}", "-".repeat(80));
            header_printed = true;
        }
        match r {
            DiffRow::Equal => unreachable!(),
            DiffRow::Differ { rust, ffi } => {
                println!(
                    "  {:>5} | {:<28} | {:<28} | DIFFER",
                    idx,
                    fmt_rust(rust),
                    fmt_ffi(ffi),
                );
            }
            DiffRow::RustOnly(rust) => {
                println!(
                    "  {:>5} | {:<28} | {:<28} | RUST_ONLY",
                    idx,
                    fmt_rust(rust),
                    "—",
                );
            }
            DiffRow::FfiOnly(ffi) => {
                println!(
                    "  {:>5} | {:<28} | {:<28} | FFI_ONLY",
                    idx,
                    "—",
                    fmt_ffi(ffi),
                );
            }
        }
        printed += 1;
    }
    if printed == 0 {
        println!("  (no divergences — sequence streams match)");
    }
}

fn fmt_rust(s: &CapturedRawSequence) -> String {
    format!(
        "{}:{:>3} {}/{}/{}",
        s.block_idx, s.seq_in_block, s.ll, s.of, s.ml
    )
}

fn fmt_ffi(s: &FfiSeq) -> String {
    format!(
        "{}:{:>3} {}/{}/{}",
        s.block_idx, s.seq_in_block, s.ll, s.of, s.ml
    )
}

/// Align two ordered sequence streams by cumulative input-bytes
/// consumed. The encoder semantics make `Σ (ll + ml)` strictly
/// non-decreasing on both sides — at each step we advance whichever
/// side has the smaller cumulative position, classifying as RustOnly /
/// FfiOnly. Equal positions are compared field-by-field and emit
/// `Equal` or `Differ`. The output ordering matches the input order.
fn align_and_diff(rust: &[CapturedRawSequence], ffi: &[FfiSeq]) -> Vec<DiffRow> {
    let mut rust_iter = rust.iter().peekable();
    let mut ffi_iter = ffi.iter().peekable();
    let mut rust_pos: u64 = 0;
    let mut ffi_pos: u64 = 0;
    let mut out = Vec::with_capacity(rust.len().max(ffi.len()));
    loop {
        match (rust_iter.peek(), ffi_iter.peek()) {
            (None, None) => break,
            (Some(r), None) => {
                let r = **r;
                rust_pos += r.ll as u64 + r.ml as u64;
                out.push(DiffRow::RustOnly(r));
                rust_iter.next();
            }
            (None, Some(f)) => {
                let f = **f;
                ffi_pos += f.ll as u64 + f.ml as u64;
                out.push(DiffRow::FfiOnly(f));
                ffi_iter.next();
            }
            (Some(r), Some(f)) => {
                let r_next_pos = rust_pos + r.ll as u64 + r.ml as u64;
                let f_next_pos = ffi_pos + f.ll as u64 + f.ml as u64;
                if r_next_pos == f_next_pos {
                    let r = **r;
                    let f = **f;
                    if r.ll == f.ll && r.of == f.of && r.ml == f.ml {
                        out.push(DiffRow::Equal);
                    } else {
                        out.push(DiffRow::Differ { rust: r, ffi: f });
                    }
                    rust_pos = r_next_pos;
                    ffi_pos = f_next_pos;
                    rust_iter.next();
                    ffi_iter.next();
                } else if r_next_pos < f_next_pos {
                    let r = **r;
                    rust_pos = r_next_pos;
                    out.push(DiffRow::RustOnly(r));
                    rust_iter.next();
                } else {
                    let f = **f;
                    ffi_pos = f_next_pos;
                    out.push(DiffRow::FfiOnly(f));
                    ffi_iter.next();
                }
            }
        }
    }
    out
}

/// Call `ZSTD_generateSequences` on `input` at `level` and return
/// non-delimiter sequences tagged with their block index.
///
/// Donor semantics (from `zstd-sys` bindgen doc): every block ends
/// with a dummy entry `(of=0, ml=0, ll=trailing_literals)`. We use
/// those entries solely to advance `current_block` and drop them from
/// the returned vector — they have no analogue in the matcher-side
/// `Sequence::Triple` stream.
fn ffi_generate_sequences(input: &[u8], level: i32) -> Vec<FfiSeq> {
    use zstd::zstd_safe::zstd_sys;
    // SAFETY: standard libzstd handle creation; null on OOM.
    let cctx = unsafe { zstd_sys::ZSTD_createCCtx() };
    assert!(!cctx.is_null(), "ZSTD_createCCtx returned null");
    let bound = unsafe { zstd_sys::ZSTD_sequenceBound(input.len()) };
    let mut buf: Vec<zstd_sys::ZSTD_Sequence> = Vec::with_capacity(bound);
    let nb_seqs = unsafe {
        let rc = zstd_sys::ZSTD_CCtx_setParameter(
            cctx,
            zstd_sys::ZSTD_cParameter::ZSTD_c_compressionLevel,
            level,
        );
        assert!(zstd_sys::ZSTD_isError(rc) == 0, "setParameter level failed");
        let n = zstd_sys::ZSTD_generateSequences(
            cctx,
            buf.as_mut_ptr(),
            bound,
            input.as_ptr() as *const core::ffi::c_void,
            input.len(),
        );
        assert!(
            zstd_sys::ZSTD_isError(n) == 0,
            "ZSTD_generateSequences failed: rc={n}"
        );
        n
    };
    // SAFETY: libzstd populated the first `nb_seqs` entries.
    unsafe { buf.set_len(nb_seqs) };
    // SAFETY: cctx is non-null and was created by us.
    unsafe { zstd_sys::ZSTD_freeCCtx(cctx) };

    let mut out = Vec::with_capacity(buf.len());
    let mut current_block: u32 = 0;
    let mut seq_in_block: u32 = 0;
    for s in &buf {
        if s.offset == 0 && s.matchLength == 0 {
            // Block delimiter — advance to next block; do not emit.
            current_block = current_block.saturating_add(1);
            seq_in_block = 0;
            continue;
        }
        out.push(FfiSeq {
            block_idx: current_block,
            seq_in_block,
            ll: s.litLength,
            of: s.offset,
            ml: s.matchLength,
        });
        seq_in_block = seq_in_block.saturating_add(1);
    }
    out
}
