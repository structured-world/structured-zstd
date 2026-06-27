//! Sequence-stream comparator: structured-zstd (pure Rust) vs zstd (C FFI).
//!
//! For a fixed `(input, level)` pair, capture the encoder sequence stream
//! from both sides and diff `(literal_length, offset, match_length)`
//! triples. The tool emits raw diff verdicts —
//! `Equal` / `Differ` / `RustOnly` / `FfiOnly` — over which a human
//! triages residual compression-ratio gaps into interpretation classes
//! ("algorithmic win" where Rust ships a sequence upstream zstd missed,
//! "cost source" where both sides emit at the same position but Rust
//! chose a worse offset/length, "missed match upstream skipped" where
//! upstream zstd emits a sequence Rust didn't). The classification labels are
//! HUMAN-APPLIED reasoning on top of the raw verdicts, not tool output.
//!
//! Pipeline:
//! 1. Rust side — drive the production [`FrameCompressor`] via
//!    [`structured_zstd::encoding::sequence_capture::compress_and_collect_sequences`].
//!    Returns a `SequenceCapture { sequences, block_tail_lengths }`
//!    where each `CapturedRawSequence { block_idx, seq_in_block, ll, of, ml }`
//!    corresponds to one `Sequence::Triple` and `block_tail_lengths[i]`
//!    holds the trailing-literal byte count for block `i` (the bytes
//!    between the last triple and the block end).
//! 2. FFI side — `ZSTD_generateSequences` against the same `level`. The
//!    upstream zstd emits a per-block stream where every block ends with a
//!    dummy delimiter `(of=0, ml=0, ll=trailing_literals)`. The
//!    delimiter's `ll` is captured as the FFI-side trailing-literal
//!    length; remaining sequences are tagged with their block index.
//! 3. Alignment — both streams are in input-order. Walk in lockstep by
//!    cumulative consumed bytes (`Σ ll + ml` per triple plus the
//!    block-tail length at each block boundary, on both sides). At
//!    each step the side with the smaller cumulative position
//!    advances; equal positions are compared directly. Without the
//!    per-block tail lengths the cumulative counter would undercount
//!    after every block with trailing literals and shift every
//!    subsequent row of the diff (PR #149 review).
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

// The `support/` module shared with `compare_ffi.rs` /
// `compare_ffi_memory.rs` is intentionally NOT declared here — this
// bench doesn't use any of its helpers, and declaring the module
// would compile the entire shared bench harness and couple this
// diagnostic tool to unrelated changes (PR #149 review #18). Cargo
// does not auto-include sibling modules in `benches/`, so leaving
// the declaration out is the correct way to opt out.

use std::fs;
use std::path::Path;

use structured_zstd::encoding::CompressionLevel;
use structured_zstd::encoding::sequence_capture::{
    CapturedRawSequence, compress_and_collect_sequences, compress_and_collect_sequences_with_dict,
    compress_and_collect_sequences_with_raw_content,
};

/// Default level set when `STRUCTURED_ZSTD_BENCH_LEVEL` is unset.
/// Single Level(3) (Dfast, the user-visible `Default` preset and
/// the focus of the Lane A `7-compress-default` sub-phase) keeps
/// the no-arg path fast for the common per-strategy audit.
///
/// Override via `STRUCTURED_ZSTD_BENCH_LEVEL`:
///
/// * `STRUCTURED_ZSTD_BENCH_LEVEL=1` — single level
/// * `STRUCTURED_ZSTD_BENCH_LEVEL=1-15` — inclusive range sweep
/// * `STRUCTURED_ZSTD_BENCH_LEVEL=1,3,7,11,15` — explicit list
/// * `STRUCTURED_ZSTD_BENCH_LEVEL=all` — every supported numeric
///   level (`1..=15`); `Level(>=16)` is rejected by the post-split
///   guard in `sequence_capture`.
const DEFAULT_LEVELS: &[i32] = &[3];

/// Highest numeric level the matcher capture supports. `Level(>=16)`
/// is rejected by `sequence_capture` because
/// `compress_block_with_post_split` emits multiple physical blocks
/// per matcher call, which the per-matcher-call block counter
/// cannot track. Bump this when per-physical-block hooks land.
const MAX_SUPPORTED_LEVEL: i32 = 15;

/// Cap on diverging-row output per fixture in single-level mode.
/// Override at runtime via `STRUCTURED_ZSTD_BENCH_MAX_ROWS`. In
/// sweep mode (multiple levels) defaults to 0 — per-level summary
/// lines only — so the output stays scannable across all levels.
const DEFAULT_MAX_DIVERGENCE_ROWS: usize = 30;

/// One sequence captured from the upstream zstd (`ZSTD_generateSequences`).
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
    let levels = parse_levels_env();
    let single = levels.len() == 1;
    let default_rows = if single {
        DEFAULT_MAX_DIVERGENCE_ROWS
    } else {
        0
    };
    let max_rows = std::env::var("STRUCTURED_ZSTD_BENCH_MAX_ROWS")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(default_rows);
    // Optional dictionary: `STRUCTURED_ZSTD_BENCH_DICT=<path>` attaches
    // the (serialized or raw-content) dict to BOTH sides before capturing
    // sequences, so the diff reflects the dict-primed match decisions.
    let dict_bytes: Option<Vec<u8>> = std::env::var("STRUCTURED_ZSTD_BENCH_DICT")
        .ok()
        .map(|p| fs::read(p.trim()).expect("read STRUCTURED_ZSTD_BENCH_DICT path"));
    let fixtures = collect_fixtures();
    println!(
        "=== compare_ffi_sequences (levels={}, dict={}, Rust vs C FFI) ===",
        fmt_levels(&levels),
        if dict_bytes.is_some() { "yes" } else { "no" },
    );
    println!();
    for (name, bytes) in &fixtures {
        println!("=== fixture: {name} ===");
        for &level in &levels {
            run_one(name, bytes, level, max_rows, dict_bytes.as_deref());
        }
        println!();
    }
}

/// Corpus-file resolution order for a decodecorpus fixture, mirroring
/// `support::load_decode_corpus_scenario`: explicit env path (its directory for
/// fixtures other than the one it names), `CARGO_MANIFEST_DIR/decodecorpus_files`,
/// then repo-relative fallbacks — so every fixture resolves under cargo-driven,
/// prebuilt-binary, and hand-run layouts alike.
fn corpus_candidates(fname: &str) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    if let Ok(explicit) = std::env::var("STRUCTURED_ZSTD_BENCH_CORPUS_PATH") {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            let explicit = std::path::PathBuf::from(trimmed);
            // The env var names a single corpus file (z000033); reuse it
            // directly for that fixture and its parent directory for the others.
            if explicit.file_name().is_some_and(|n| n == fname) {
                paths.push(explicit);
            } else if let Some(dir) = explicit.parent() {
                paths.push(dir.join(fname));
            }
        }
    }
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        paths.push(
            Path::new(&manifest_dir)
                .join("decodecorpus_files")
                .join(fname),
        );
    }
    paths.push(std::path::PathBuf::from("decodecorpus_files").join(fname));
    paths.push(std::path::PathBuf::from("zstd/decodecorpus_files").join(fname));
    paths
}

/// Split an inclusive `lo-hi` range on its separating `-` — the first `-`
/// preceded by a digit — so negative bounds parse correctly (`-7--1` → `-7`,
/// `-1`; `1-9` → `1`, `9`). Returns `None` when no digit-preceded separator
/// exists.
fn split_inclusive_range(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    let sep = (1..bytes.len()).find(|&i| bytes[i] == b'-' && bytes[i - 1].is_ascii_digit())?;
    Some((&s[..sep], &s[sep + 1..]))
}

/// Parse `STRUCTURED_ZSTD_BENCH_LEVEL` env var into a level list.
///
/// Forms accepted: single (`3`), range (`1-15`), comma list
/// (`1,3,7,11,15`), keyword `all` (= `1..=MAX_SUPPORTED_LEVEL`).
/// Empty / unset / unparseable → [`DEFAULT_LEVELS`]. Levels above
/// `MAX_SUPPORTED_LEVEL` (post-split territory) are silently
/// filtered out with a stderr warning so a careless `all` does not
/// blow up on the sequence_capture guard.
fn parse_levels_env() -> Vec<i32> {
    let raw = match std::env::var("STRUCTURED_ZSTD_BENCH_LEVEL") {
        Ok(v) => v,
        Err(_) => return DEFAULT_LEVELS.to_vec(),
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return DEFAULT_LEVELS.to_vec();
    }
    if trimmed.eq_ignore_ascii_case("all") {
        // Include the negative band (the production compressor handles it, and
        // the per-level parser above now accepts it) so "run everything" does
        // not silently skip the negative-level coverage. `0` is not a level.
        return (-7..=MAX_SUPPORTED_LEVEL).filter(|&l| l != 0).collect();
    }
    // Order matters because a negative level's leading `-` must not be read as
    // an empty range bound:
    //   1. bare integer (incl. a single negative like `-5`),
    //   2. comma list (items may be negative: `-7,-5`, `3,-1`) — checked BEFORE
    //      the range split, which would otherwise swallow the list's minus,
    //   3. inclusive `lo-hi` range on the digit-preceded separator so negative
    //      bounds (`-7--1`) parse correctly.
    let parsed: Vec<i32> = if let Ok(single) = trimmed.parse::<i32>() {
        vec![single]
    } else if trimmed.contains(',') {
        trimmed
            .split(',')
            .filter_map(|s| s.trim().parse::<i32>().ok())
            .collect()
    } else if let Some((lo, hi)) = split_inclusive_range(trimmed) {
        match (lo.parse::<i32>(), hi.parse::<i32>()) {
            (Ok(a), Ok(b)) if a <= b => (a..=b).collect(),
            _ => {
                eprintln!(
                    "warn: STRUCTURED_ZSTD_BENCH_LEVEL={trimmed:?} is not a valid range; \
                     falling back to default levels {DEFAULT_LEVELS:?}",
                );
                return DEFAULT_LEVELS.to_vec();
            }
        }
    } else {
        eprintln!(
            "warn: STRUCTURED_ZSTD_BENCH_LEVEL={trimmed:?} is not a level, list, or range; \
             falling back to default levels {DEFAULT_LEVELS:?}",
        );
        return DEFAULT_LEVELS.to_vec();
    };
    let (supported, dropped): (Vec<i32>, Vec<i32>) = parsed
        .into_iter()
        .partition(|&l| ((-7..=MAX_SUPPORTED_LEVEL).contains(&l)) && l != 0);
    if !dropped.is_empty() {
        eprintln!(
            "warn: dropping unsupported levels {dropped:?} (supported numeric levels \
             are 1..={MAX_SUPPORTED_LEVEL}; 0/negatives unsupported by sequence_capture, \
             >={post_split} rejected by post-split guard)",
            post_split = MAX_SUPPORTED_LEVEL + 1,
        );
    }
    if supported.is_empty() {
        eprintln!(
            "warn: STRUCTURED_ZSTD_BENCH_LEVEL={trimmed:?} yielded no supported levels; \
             falling back to default {DEFAULT_LEVELS:?}",
        );
        return DEFAULT_LEVELS.to_vec();
    }
    supported
}

fn fmt_levels(levels: &[i32]) -> String {
    if levels.len() <= 4 {
        levels
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join(",")
    } else {
        let first = levels.first().copied().unwrap_or(0);
        let last = levels.last().copied().unwrap_or(0);
        let contiguous = levels.len() == (last - first + 1) as usize
            && levels
                .iter()
                .enumerate()
                .all(|(i, &l)| l == first + i as i32);
        if contiguous {
            format!("{first}-{last}")
        } else {
            format!(
                "{} ({} levels)",
                levels
                    .iter()
                    .take(3)
                    .map(|l| l.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
                levels.len(),
            )
        }
    }
}

/// Build the fixture set. Returns `(name, bytes)` pairs ready to feed
/// into both encoders. The lookup is intentionally permissive — if the
/// disk fixture is missing (e.g. `decodecorpus_files/` excluded from a
/// downstream packaging), the synthetic fixture still runs so the
/// bench is never a hard failure on a fresh checkout.
fn collect_fixtures() -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();

    // Mirror the corpus resolution order used by
    // `zstd/benches/support/mod.rs::load_decode_corpus_scenario` so
    // this bench finds the canonical `decodecorpus-z000033` fixture
    // under the same conditions as `compare_ffi.rs` /
    // `compare_ffi_memory.rs`:
    //   1. `STRUCTURED_ZSTD_BENCH_CORPUS_PATH` — explicit path. CI
    //      sets this when invoking the prebuilt bench binary
    //      directly (no `CARGO_MANIFEST_DIR` in that environment).
    //   2. `CARGO_MANIFEST_DIR/decodecorpus_files/z000033` —
    //      cargo-driven `cargo bench` runs.
    //   3. Repo-relative fallback for hand-run binaries from the
    //      crate dir or workspace root.
    // Without (1) and (2), the canonical fixture would silently
    // skip on CI runs that bypass cargo, undermining the audit
    // (PR #149 review round 3 #11).
    let mut found = false;
    for p in &corpus_candidates("z000033") {
        if let Ok(bytes) = fs::read(p)
            && !bytes.is_empty()
        {
            out.push((format!("z000033 ({} bytes)", bytes.len()), bytes));
            found = true;
            break;
        }
    }
    if !found {
        eprintln!(
            "warn: decodecorpus z000033 not found via STRUCTURED_ZSTD_BENCH_CORPUS_PATH, \
             CARGO_MANIFEST_DIR, or repo-relative paths — skipping",
        );
    }

    // Smaller decodecorpus frames (single-block) where the negative/fast band
    // over-compresses vs C — these are the ratio-parity targets.
    for fname in ["z000002", "z000000"] {
        // Same resolution order as z000033 (explicit env path, CARGO_MANIFEST_DIR,
        // repo-relative) so these targets are not silently skipped under the
        // direct-binary / repo-root layouts.
        for p in &corpus_candidates(fname) {
            if let Ok(bytes) = fs::read(p)
                && !bytes.is_empty()
            {
                out.push((format!("{fname} ({} bytes)", bytes.len()), bytes));
                break;
            }
        }
    }

    let log = build_low_entropy_log(16 * 1024);
    out.push((format!("low-entropy log ({} bytes)", log.len()), log));

    // Byte-for-byte the bench `small-4k-log-lines` scenario so the
    // dict-primed diff reproduces the L3 dfast compress-dict ratio gap
    // exactly (pair with a dict trained on the same 4 log lines via
    // `STRUCTURED_ZSTD_BENCH_DICT`).
    let logs4k = repeated_log_lines(4096);
    out.push((
        format!("small-4k-log-lines ({} bytes)", logs4k.len()),
        logs4k,
    ));

    // 1 KiB variant — the `dict_matrix` `logs-1k` case that carries the L6
    // Row/lazy dict ratio gap (26 vs 18 bytes; #434).
    let logs1k = repeated_log_lines(1024);
    out.push((
        format!("small-1k-log-lines ({} bytes)", logs1k.len()),
        logs1k,
    ));

    out
}

/// Byte-for-byte the bench `repeated_log_lines(len)` fixture (and the
/// `encode_loop_dict` example's `logs<N>` input).
fn repeated_log_lines(len: usize) -> Vec<u8> {
    const LINES: &[&str] = &[
        "ts=2026-03-26T21:39:28Z level=INFO msg=\"flush memtable\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:29Z level=INFO msg=\"rotate segment\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:30Z level=INFO msg=\"compact level\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:31Z level=INFO msg=\"write block\" tenant=demo table=orders region=eu-west\n",
    ];
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        for line in LINES {
            if bytes.len() == len {
                break;
            }
            let remaining = len - bytes.len();
            bytes.extend_from_slice(&line.as_bytes()[..line.len().min(remaining)]);
        }
    }
    bytes
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
        let elapsed = (counter % 1000).to_string();
        out.extend_from_slice(elapsed.as_bytes());
        out.push(b'\n');
        counter = counter.wrapping_add(1);
    }
    out.truncate(byte_budget);
    out
}

fn run_one(_name: &str, input: &[u8], level: i32, max_rows: usize, dict: Option<&[u8]>) {
    let rust_capture = match dict {
        // Auto-detect the dict kind the same way the C side's
        // `ZSTD_CCtx_loadDictionary` does: a serialized dict (zstd dict magic
        // `0xEC30A437`, LE bytes `37 A4 30 EC`) attaches via the magic path;
        // anything else is a raw-content dict (the per-label-dict use case).
        Some(d) if d.starts_with(&[0x37, 0xA4, 0x30, 0xEC]) => {
            compress_and_collect_sequences_with_dict(input, CompressionLevel::Level(level), d)
        }
        Some(d) => compress_and_collect_sequences_with_raw_content(
            input,
            CompressionLevel::Level(level),
            d,
        ),
        None => compress_and_collect_sequences(input, CompressionLevel::Level(level)),
    };
    let (ffi_seqs, ffi_tail_lengths) = ffi_generate_sequences(input, level, dict);
    let rust_seqs = &rust_capture.sequences;
    let rust_tail_lengths = &rust_capture.block_tail_lengths;

    let rows = align_and_diff(rust_seqs, rust_tail_lengths, &ffi_seqs, &ffi_tail_lengths);
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
    // Wording note: `Differ/RustOnly/FfiOnly` are RAW classifications,
    // not value judgments. Divergence from the upstream zstd is not a bug —
    // structured-zstd is allowed (and expected) to make different
    // and sometimes better choices. The tool surfaces "where do we
    // pick a different path"; whether each path is a win or
    // regression is a human-applied call after looking at the
    // emitted bytes.
    // `differ` is the `DiffRow::Differ` bucket only — both sides emit
    // a triple at the same cumulative position but with different
    // `(ll, of, ml)`. `rust_only` / `ffi_only` are separate buckets
    // (one side emits a triple where the other doesn't). They are
    // all forms of divergence; the label uses the enum name so a
    // reader can map each column back to the `DiffRow` variant
    // instead of conflating `Differ` with the total.
    println!(
        "  level={level:>2}  rust_seqs={rs:>6} ({rb} blk)  ffi_seqs={fs:>6} ({fb} blk)  \
         match={equal:>6} ({pct:.1}%)  differ={dv:>6}  rust_only={rust_only:>5}  \
         ffi_only={ffi_only}",
        rs = rust_seqs.len(),
        rb = rust_tail_lengths.len(),
        fs = ffi_seqs.len(),
        fb = ffi_tail_lengths.len(),
        dv = differ,
        pct = equal as f64 * 100.0 / total as f64,
    );

    if max_rows == 0 {
        return;
    }
    let mut printed = 0usize;
    let mut header_printed = false;
    for (idx, r) in rows.iter().enumerate() {
        if matches!(r, DiffRow::Equal) {
            continue;
        }
        if printed >= max_rows {
            println!(
                "    ... (omitted {} more diverging rows; raise STRUCTURED_ZSTD_BENCH_MAX_ROWS to see them)",
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

/// Apply the per-block trailing-literal length for every block whose
/// last triple has already been consumed. `current_block_idx` walks
/// forward as the iterator advances; whenever we step into a new
/// block, the previous block's tail bytes belong to the consumed
/// span. Done as a helper so both Rust and FFI sides apply the same
/// rule without duplicating the bookkeeping.
fn advance_tail_for_completed_blocks(
    pos: &mut u64,
    current_block_idx: &mut u32,
    next_seq_block: u32,
    tail_lengths: &[u32],
) {
    while *current_block_idx < next_seq_block {
        if let Some(&tail) = tail_lengths.get(*current_block_idx as usize) {
            *pos += tail as u64;
        }
        *current_block_idx = current_block_idx.saturating_add(1);
    }
}

/// Align two ordered sequence streams by cumulative input-bytes
/// consumed. The encoder semantics make the running position
/// (`Σ (ll + ml)` across triples PLUS per-block trailing-literal
/// length applied at each block boundary) strictly non-decreasing on
/// both sides. Per-block tails MUST be applied or multi-block
/// fixtures undercount after every block with trailing literals,
/// shifting every subsequent diff row (PR #149 review #1-#3).
///
/// At each step we advance whichever side has the smaller cumulative
/// position, classifying as `RustOnly` / `FfiOnly`. Equal positions
/// are compared field-by-field and emit `Equal` or `Differ`. The
/// cumulative counters are internal state — they drive the
/// per-iteration "which side is behind" decision and the final
/// `Some(_, None)` / `(None, Some(_))` drain logic, but are NOT
/// returned and the caller's printed summary does not assert
/// equality against them. The fail-fast invariants live on the
/// capture/FFI sides (`compress_and_collect_sequences` +
/// `ffi_generate_sequences`); reaching this function with
/// undercounted tails would be caught upstream.
fn align_and_diff(
    rust: &[CapturedRawSequence],
    rust_tails: &[u32],
    ffi: &[FfiSeq],
    ffi_tails: &[u32],
) -> Vec<DiffRow> {
    let mut rust_iter = rust.iter().peekable();
    let mut ffi_iter = ffi.iter().peekable();
    let mut rust_pos: u64 = 0;
    let mut ffi_pos: u64 = 0;
    let mut rust_block_idx: u32 = 0;
    let mut ffi_block_idx: u32 = 0;
    let mut out = Vec::with_capacity(rust.len().max(ffi.len()));
    loop {
        // Before peeking at the next triple, apply tails for every
        // block whose final triple was already consumed but whose
        // tail bytes haven't been counted yet. This keeps the
        // cumulative position aligned with on-wire byte consumption.
        // Flush pending tails for every block whose final triple was
        // already consumed. When `peek()` is `None`, advance to
        // `tails.len()` so the FINAL block's tail (always present for
        // any non-trivial input) AND any literal-only / RLE-routed
        // blocks past the iterator's end are counted. Without the
        // `unwrap_or(tails.len())` branch the final block's tail
        // would silently never be added to the cumulative cursor
        // (PR #149 review round 2 #5).
        let rust_next_block = rust_iter
            .peek()
            .map(|r| r.block_idx)
            .unwrap_or(rust_tails.len() as u32);
        advance_tail_for_completed_blocks(
            &mut rust_pos,
            &mut rust_block_idx,
            rust_next_block,
            rust_tails,
        );
        let ffi_next_block = ffi_iter
            .peek()
            .map(|f| f.block_idx)
            .unwrap_or(ffi_tails.len() as u32);
        advance_tail_for_completed_blocks(
            &mut ffi_pos,
            &mut ffi_block_idx,
            ffi_next_block,
            ffi_tails,
        );
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
                // Compare field-by-field ONLY when both streams are at
                // the same current byte position. Gating on
                // `r_next_pos == f_next_pos` would collapse an
                // "extra sequence on one side + catch-up on the other"
                // case into a single misleading `Differ` row whenever
                // the summed (ll + ml) deltas happen to balance — the
                // two triples were emitted at different input
                // positions and aren't actually comparable
                // (PR #149 review round 2 #6).
                if rust_pos == ffi_pos {
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
                } else if rust_pos < ffi_pos {
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
/// non-delimiter sequences tagged with their block index, plus a
/// parallel vec of trailing-literal lengths indexed by block.
///
/// Upstream zstd semantics (from `zstd-sys` bindgen doc): every block ends
/// with a dummy entry `(of=0, ml=0, ll=trailing_literals)`. The
/// dummy's `litLength` is the bytes between the last real triple of
/// the block and the block end — needed by the comparator's
/// cumulative-position alignment to advance the FFI counter at each
/// block boundary by the same amount the on-wire encoding consumes.
/// Without this, FFI runs behind Rust by exactly the trailing-literal
/// count after every block boundary on multi-block inputs (PR #149
/// review #1-#3).
fn ffi_generate_sequences(
    input: &[u8],
    level: i32,
    dict: Option<&[u8]>,
) -> (Vec<FfiSeq>, Vec<u32>) {
    use zstd::zstd_safe::{self, zstd_sys};
    // Mirror of `assert_zstd_ok` in `encoding/match_generator.rs` —
    // surfaces libzstd's symbolic error name in the panic message
    // instead of a raw numeric return code, so a triage glance at
    // the bench log says e.g. "Parameter unsupported" rather than
    // "rc=18446744073709551614".
    fn assert_zstd_ok(code: usize, context: &str) {
        assert_eq!(
            unsafe { zstd_sys::ZSTD_isError(code) },
            0,
            "{context} failed: {}",
            zstd_safe::get_error_name(code)
        );
    }
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
        assert_zstd_ok(rc, "ZSTD_CCtx_setParameter(ZSTD_c_compressionLevel)");
        // Mirror the small-input windowLog=14 override used by
        // `compare_ffi.rs` so the upstream zstd parser sees the same window
        // the Rust encoder applies when the source size is hinted
        // down to a 16 KiB frame. Without this, tiny fixtures
        // (e.g. the 16 KiB synthetic log) get parsed against a
        // larger default window on the FFI side and produce
        // sequence-stream divergences that are pure window-mismatch
        // artifacts, not real strategy differences.
        if input.len() <= (1 << 14) {
            let rc = zstd_sys::ZSTD_CCtx_setParameter(
                cctx,
                zstd_sys::ZSTD_cParameter::ZSTD_c_windowLog,
                14,
            );
            assert_zstd_ok(rc, "ZSTD_CCtx_setParameter(ZSTD_c_windowLog=14)");
        }
        // Attach the dictionary so the upstream zstd parser sees the same primed
        // state the Rust `set_dictionary_from_bytes` path applies.
        // `ZSTD_CCtx_loadDictionary` auto-detects a serialized dict
        // (magic) vs raw content and seeds the matcher tables, offset
        // history, and entropy tables — the same priming the Rust
        // capture's dict variant reproduces.
        if let Some(d) = dict {
            let rc = zstd_sys::ZSTD_CCtx_loadDictionary(
                cctx,
                d.as_ptr() as *const core::ffi::c_void,
                d.len(),
            );
            assert_zstd_ok(rc, "ZSTD_CCtx_loadDictionary");
        }
        let n = zstd_sys::ZSTD_generateSequences(
            cctx,
            buf.as_mut_ptr(),
            bound,
            input.as_ptr() as *const core::ffi::c_void,
            input.len(),
        );
        assert_zstd_ok(n, "ZSTD_generateSequences");
        n
    };
    // Defensive guard: `set_len(nb_seqs)` past the allocated capacity
    // would expose uninitialized memory if libzstd ever returned more
    // sequences than `ZSTD_sequenceBound` reserved space for. The
    // bound is documented as inclusive but the check is one assert
    // (PR #149 review #16).
    assert!(
        nb_seqs <= bound,
        "ZSTD_generateSequences returned more sequences ({nb_seqs}) than ZSTD_sequenceBound reserved ({bound})",
    );
    // SAFETY: libzstd populated the first `nb_seqs` entries, and the
    // assert above guarantees `nb_seqs <= capacity`.
    unsafe { buf.set_len(nb_seqs) };
    // SAFETY: cctx is non-null and was created by us.
    unsafe { zstd_sys::ZSTD_freeCCtx(cctx) };

    let mut out = Vec::with_capacity(buf.len());
    let mut tails = Vec::new();
    let mut current_block: u32 = 0;
    let mut seq_in_block: u32 = 0;
    for s in &buf {
        if s.offset == 0 && s.matchLength == 0 {
            // Block delimiter — record trailing-literal length for
            // this block, then advance the block counter. The
            // delimiter is not emitted as an `FfiSeq` because it has
            // no analogue on the Rust matcher side.
            tails.push(s.litLength);
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
    // Mirror the Rust-side fail-fast invariant in
    // `compress_and_collect_sequences`: if `ZSTD_generateSequences`
    // omits a block delimiter or our filter misses one, `tails` ends
    // up short and `align_and_diff` would walk a stale cursor and
    // emit misleading rows. Panic with a clear FFI-specific message
    // so the broken precondition surfaces immediately instead of
    // being masked as a "real" divergence (PR #149 review round 3 #8).
    let reconstructed: u64 = out.iter().map(|s| s.ll as u64 + s.ml as u64).sum::<u64>()
        + tails.iter().map(|t| *t as u64).sum::<u64>();
    assert_eq!(
        reconstructed,
        input.len() as u64,
        "ffi_generate_sequences: stream undercounted input bytes — \
         Σ(ll+ml)+Σ(tails)={reconstructed}, input.len()={}. \
         Likely cause: a `ZSTD_generateSequences` block delimiter \
         was omitted or filtered incorrectly.",
        input.len(),
    );
    (out, tails)
}
