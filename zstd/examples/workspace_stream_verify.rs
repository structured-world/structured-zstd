//! Verify a compressed blob in a bounded amount of memory before using it.
//!
//! The workspace is admitted up front from the limits and allocated fallibly;
//! the blob is decoded in fixed chunks through a reusable output buffer, and
//! every produced chunk is counted and hashed instead of collected. The blob is
//! accepted only when the stream ends cleanly, the size is exact and the digest
//! matches; any error rejects it with nothing partial kept.
//!
//! A real consumer would use its own digest (SHA-256, say); FNV-1a keeps this
//! example free of dependencies.
//!
//! ```text
//! cargo run --example workspace_stream_verify --features caller-workspace
//! ```

use structured_zstd::decoding::workspace::{
    DecodeLimits, StepState, WorkBudget, WorkspaceDecodeError, WorkspaceDecoder, workspace_bytes,
};
use structured_zstd::encoding::{CompressionLevel, compress_slice_to_vec};

/// What the manifest says about the blob.
struct Manifest {
    size: u64,
    digest: u64,
}

#[derive(Debug)]
enum Rejected {
    /// The workspace could not be admitted.
    Memory,
    /// The decoder refused the stream (limit, format, truncation, ...).
    Decode(WorkspaceDecodeError),
    /// The stream decoded, but not to the manifest's size or digest.
    Mismatch,
}

impl core::fmt::Display for Rejected {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Rejected::Memory => write!(f, "workspace not admitted"),
            Rejected::Decode(e) => write!(f, "decode refused: {e}"),
            Rejected::Mismatch => write!(f, "size or digest differs from the manifest"),
        }
    }
}

/// How a verification ended, for printing.
fn outcome(result: Result<(), Rejected>) -> String {
    match result {
        Ok(()) => "accepted".to_string(),
        Err(e) => format!("rejected ({e})"),
    }
}

struct Fnv1a(u64);

impl Fnv1a {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }
}

fn verify(compressed: &[u8], manifest: &Manifest, max_window: u64) -> Result<(), Rejected> {
    // 1. Size the workspace from the limits and admit it before decoding.
    let limits = DecodeLimits::new(max_window).with_max_output(manifest.size);
    let needed = workspace_bytes(limits.max_window_size).map_err(|_| Rejected::Memory)?;
    let mut workspace = Vec::new();
    workspace
        .try_reserve_exact(needed)
        .map_err(|_| Rejected::Memory)?;
    workspace.resize(needed, 0);
    let mut decoder =
        WorkspaceDecoder::new(&mut workspace, limits).map_err(|_| Rejected::Memory)?;

    // 2. Feed chunks, hash and count each produced slice.
    let mut work = WorkBudget::new(16 * (compressed.len() as u64 + manifest.size) + (1 << 20));
    let mut out = [0u8; 16 * 1024];
    let mut hasher = Fnv1a::new();
    let mut produced = 0u64;
    let mut input = compressed;
    loop {
        let chunk = &input[..input.len().min(64 * 1024)];
        let last = chunk.len() == input.len();
        let progress = decoder
            .step(chunk, last, &mut out, &mut work)
            .map_err(Rejected::Decode)?;
        hasher.update(&out[..progress.produced]);
        produced += progress.produced as u64;
        input = &input[progress.consumed..];
        if progress.state == StepState::Done {
            break;
        }
    }

    // 3. Exact size and digest, then commit.
    if produced != manifest.size || hasher.0 != manifest.digest {
        return Err(Rejected::Mismatch);
    }
    Ok(())
}

fn main() {
    let content: Vec<u8> = (0..2_000_000u32)
        .map(|i| (i % 251) as u8 ^ (i >> 11) as u8)
        .collect();
    let compressed = compress_slice_to_vec(&content, CompressionLevel::Default);
    let mut hasher = Fnv1a::new();
    hasher.update(&content);
    let manifest = Manifest {
        size: content.len() as u64,
        digest: hasher.0,
    };

    verify(&compressed, &manifest, 8 << 20).expect("the blob verifies");
    println!(
        "verified {} bytes from {} compressed in a {}-byte workspace",
        manifest.size,
        compressed.len(),
        workspace_bytes(8 << 20).unwrap()
    );

    let mut corrupt = compressed.clone();
    let middle = corrupt.len() / 2;
    corrupt[middle] ^= 0x40;
    println!(
        "corrupted blob: {}",
        outcome(verify(&corrupt, &manifest, 8 << 20))
    );
    let truncated = &compressed[..compressed.len() - 10];
    println!(
        "truncated blob: {}",
        outcome(verify(truncated, &manifest, 8 << 20))
    );
}
