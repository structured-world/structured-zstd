//! Donor-parity verification for the block splitter port.
//!
//! Our `donor_split_block_from_borders` and `donor_split_block_by_chunks`
//! ports must produce byte-identical decisions to upstream donor
//! `ZSTD_splitBlock` for every (block, split_level) input in the
//! decode corpus. This test invokes both implementations on the same
//! 128 KB chunks and asserts equality.
//!
//! Donor reference: `lib/compress/zstd_preSplit.h` —
//! `ZSTD_splitBlock(blockStart, blockSize == 128 KB, level ∈ 0..=4,
//! workspace, wkspSize >= ZSTD_SLIPBLOCK_WORKSPACESIZE = 8208)`.
//! `ZSTD_splitBlock` is NOT in the public `zstd.h` API; we declare
//! the extern manually since `zstd-sys` does not bind it.
//!
//! Implements #206 (block splitter donor-parity check). Per the
//! issue acceptance criteria, divergences correlating with ratio
//! losses would justify porting more donor split logic; divergences
//! that are size-neutral represent algorithmic freedom. Today we
//! assert STRICT equality because `donor_split_block_from_borders`
//! and `donor_split_block_by_chunks` are direct ports — any
//! divergence is a porting bug, not algorithmic freedom.

use std::ffi::c_void;
use std::fs;
use std::path::PathBuf;

use structured_zstd::testing::{MAX_BLOCK_SIZE, block_splitter_decision};
// Pulls in the static libzstd archive so the linker resolves
// the manually-declared `ZSTD_splitBlock` symbol below.
use zstd::zstd_safe::zstd_sys as _;

/// Donor preSplit workspace size constant from `zstd_preSplit.h`.
const ZSTD_SLIPBLOCK_WORKSPACESIZE: usize = 8208;

// `non_snake_case` is allowed on the extern block so the donor symbol
// keeps its exact upstream spelling — the linker resolves by name, and
// renaming would force a `#[link_name = ...]` shim with no readability
// gain.
#[allow(non_snake_case)]
unsafe extern "C" {
    /// Donor `ZSTD_splitBlock` (internal, not in `zstd.h` public API
    /// but exported by libzstd). Returns the split position within
    /// `[0, blockSize)` or `blockSize` if no split is chosen.
    ///
    /// Contract per `zstd_preSplit.h`:
    /// - `blockSize` MUST equal 128 KB
    /// - `level` ∈ `0..=4` (higher = more energy on boundary detection)
    /// - `workspace` aligned for `size_t`, size ≥ `ZSTD_SLIPBLOCK_WORKSPACESIZE`
    fn ZSTD_splitBlock(
        block_start: *const c_void,
        block_size: usize,
        level: i32,
        workspace: *mut c_void,
        wksp_size: usize,
    ) -> usize;
}

/// Resolve the decode-corpus directory. The repository checks in
/// `zstd/decodecorpus_files/` as a fixture for integration tests;
/// the directory is intentionally excluded from the published
/// crates.io package (see `[package].exclude` in `Cargo.toml`), so
/// this test only runs against a repository checkout. Resolving
/// from `CARGO_MANIFEST_DIR` makes the path deterministic across
/// runners (nextest, IDE test runners, in-tree `cargo test`,
/// out-of-tree invocations) when the fixture is present.
fn corpus_dir() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("decodecorpus_files");
    assert!(
        path.is_dir(),
        "expected corpus directory at {} — this test needs the \
         decodecorpus_files/ fixture from the repository checkout; \
         it is not shipped in the crates.io package",
        path.display()
    );
    path
}

/// Load every fixture file from the decode corpus into a flat byte buffer.
/// Skips compressed `.zst` files so each fixture is a unique raw payload.
fn load_corpus_chunks() -> Vec<(String, Vec<u8>)> {
    let dir = corpus_dir();
    let mut chunks = Vec::new();
    for entry in fs::read_dir(&dir).expect("read_dir corpus") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        // Skip non-regular entries (subdirectories, symlinks-to-dirs,
        // device nodes if the fixture tree ever grows). `fs::read` on
        // a directory would panic and turn an extensible fixture
        // layout into a parity-harness failure.
        if !entry.file_type().expect("entry file_type").is_file() {
            continue;
        }
        // Skip compressed fixtures — we want raw payloads.
        if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e == "zst")
        {
            continue;
        }
        let bytes = fs::read(&path).expect("read corpus file");
        // Donor's `ZSTD_splitBlock` only accepts exactly 128 KB.
        // Stride through the corpus file in 128 KB chunks; skip the
        // tail if it's not exactly one block.
        let block_size = MAX_BLOCK_SIZE as usize;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        let mut offset = 0;
        let mut chunk_idx = 0;
        while offset + block_size <= bytes.len() {
            chunks.push((
                format!("{name}#chunk{chunk_idx}"),
                bytes[offset..offset + block_size].to_vec(),
            ));
            offset += block_size;
            chunk_idx += 1;
        }
    }
    chunks
}

/// Synthetic 128 KB chunk with a fingerprint transition at `transition_at`.
/// First `transition_at` bytes use one pseudo-random stream; the
/// rest use a different one. Probes the borders / chunks heuristic
/// at known boundary positions.
fn synthetic_transition_chunk(transition_at: usize) -> Vec<u8> {
    let block_size = MAX_BLOCK_SIZE as usize;
    let mut block = Vec::with_capacity(block_size);
    // Two distinct xorshift seeds so the byte distributions differ
    // enough for the donor histogram to register a split.
    let mut s1: u64 = 0xDEAD_BEEF_CAFE_F00D;
    let mut s2: u64 = 0x0123_4567_89AB_CDEF;
    for i in 0..block_size {
        let v = if i < transition_at {
            s1 ^= s1 << 13;
            s1 ^= s1 >> 7;
            s1 ^= s1 << 17;
            (s1 & 0xFF) as u8
        } else {
            s2 ^= s2 << 13;
            s2 ^= s2 >> 7;
            s2 ^= s2 << 17;
            (s2 & 0xFF) as u8
        };
        block.push(v);
    }
    block
}

/// Call donor `ZSTD_splitBlock` with a fresh stack-aligned workspace.
fn donor_decision(block: &[u8], level: i32) -> usize {
    assert_eq!(block.len(), MAX_BLOCK_SIZE as usize);
    // `ZSTD_SLIPBLOCK_WORKSPACESIZE` is in bytes; we allocate `u64`
    // slots so the buffer is naturally 8-byte aligned (satisfies the
    // donor's `size_t` alignment requirement on all supported targets,
    // where `size_t` is at most 8 bytes). Slot count is the ceiling
    // of `ZSTD_SLIPBLOCK_WORKSPACESIZE / size_of::<u64>()` so the
    // byte budget never under-shoots the donor minimum even if the
    // constant is later raised to a non-multiple of 8. The actual
    // byte count passed to donor below is `workspace.len() * size_of::<u64>()`.
    const U64_SIZE: usize = core::mem::size_of::<u64>();
    let workspace_slots = ZSTD_SLIPBLOCK_WORKSPACESIZE.div_ceil(U64_SIZE);
    let mut workspace = vec![0u64; workspace_slots];
    // SAFETY: block.len() == 128 KB (asserted above), level ∈ 0..=4
    // (caller-enforced in test bodies below), workspace size ≥
    // ZSTD_SLIPBLOCK_WORKSPACESIZE bytes, workspace aligned for size_t
    // (u64 backing storage).
    unsafe {
        ZSTD_splitBlock(
            block.as_ptr() as *const c_void,
            block.len(),
            level,
            workspace.as_mut_ptr() as *mut c_void,
            workspace.len() * U64_SIZE,
        )
    }
}

fn assert_parity(label: &str, block: &[u8], split_level: usize) {
    let ours = block_splitter_decision(block, split_level);
    let donor = donor_decision(block, split_level as i32);
    assert_eq!(
        ours,
        donor,
        "{label} @ split_level={split_level}: \
         our port = {ours}, donor = {donor} \
         (block first 16 bytes = {:02X?})",
        &block[..16]
    );
}

#[test]
fn corpus_borders_heuristic_matches_donor() {
    let chunks = load_corpus_chunks();
    assert!(
        !chunks.is_empty(),
        "expected at least one 128 KB chunk from the decode corpus"
    );
    for (label, block) in &chunks {
        assert_parity(label, block, 0);
    }
}

#[test]
fn corpus_by_chunks_matches_donor_at_each_sampling_level() {
    let chunks = load_corpus_chunks();
    assert!(
        !chunks.is_empty(),
        "expected at least one 128 KB chunk from the decode corpus"
    );
    for (label, block) in &chunks {
        for level in 1..=4 {
            assert_parity(label, block, level);
        }
    }
}

#[test]
fn synthetic_transition_at_32k_borders_heuristic() {
    let block = synthetic_transition_chunk(32 * 1024);
    assert_parity("synthetic-transition-32k", &block, 0);
}

#[test]
fn synthetic_transition_at_64k_borders_heuristic() {
    let block = synthetic_transition_chunk(64 * 1024);
    assert_parity("synthetic-transition-64k", &block, 0);
}

#[test]
fn synthetic_transition_at_96k_borders_heuristic() {
    let block = synthetic_transition_chunk(96 * 1024);
    assert_parity("synthetic-transition-96k", &block, 0);
}

#[test]
fn synthetic_no_transition_borders_heuristic() {
    // No transition — single-stream chunk; donor + ours should both
    // return block.len() (no split).
    let block = synthetic_transition_chunk(MAX_BLOCK_SIZE as usize);
    let split_level = 0;
    let ours = block_splitter_decision(&block, split_level);
    let donor = donor_decision(&block, split_level as i32);
    assert_eq!(ours, donor, "no-transition: ours={ours} donor={donor}");
    assert_eq!(
        ours,
        block.len(),
        "no-transition: expected no split (== block.len()), got {ours}"
    );
}

#[test]
fn synthetic_transitions_by_chunks_all_levels() {
    for &transition_at in &[16 * 1024usize, 32 * 1024, 48 * 1024, 64 * 1024, 96 * 1024] {
        let block = synthetic_transition_chunk(transition_at);
        let label = format!("synthetic-transition-{}k", transition_at / 1024);
        for level in 1..=4 {
            assert_parity(&label, &block, level);
        }
    }
}
