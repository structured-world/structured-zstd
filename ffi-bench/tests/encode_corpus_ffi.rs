//! Corpus roundtrip against the C decoder: every decodecorpus file compressed
//! with `structured-zstd` (uncompressed-level and fastest-level) must decode
//! through the C `zstd` decoder back to the original bytes. The our-decoder
//! variants stay in the library crate; these C-decoder variants live here so
//! the library never links the C bindings. The corpus directory is resolved
//! relative to the `zstd` crate regardless of the test runner's cwd.

use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;

use structured_zstd::encoding::{CompressionLevel, FrameCompressor};

const CORPUS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../zstd/decodecorpus_files");
const LOCAL_CORPUS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../zstd/local_corpus_files");

fn corpus_files() -> Vec<PathBuf> {
    // Only regular files: a stray subdirectory or symlink in the corpus dir
    // would otherwise reach `fs::read` below and panic.
    fn regular_files(dir: &str) -> impl Iterator<Item = PathBuf> {
        fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .map(|e| e.path())
    }
    let mut files: Vec<PathBuf> = regular_files(CORPUS_DIR).collect();
    files.extend(regular_files(LOCAL_CORPUS_DIR));
    files.sort();
    assert!(
        !files.is_empty(),
        "no corpus files found at {CORPUS_DIR} — the decodecorpus_files/ fixture \
         from the repository checkout is required for this test"
    );
    files
}

fn compress(input: &[u8], level: CompressionLevel) -> Vec<u8> {
    let mut compressed = Vec::new();
    let mut compressor = FrameCompressor::new(level);
    compressor.set_source(input);
    compressor.set_drain(&mut compressed);
    compressor.compress();
    compressed
}

fn roundtrip_corpus_through_c_decoder(level: CompressionLevel) {
    let mut failures: Vec<(PathBuf, String)> = Vec::new();
    for path in corpus_files() {
        if path.extension() == Some(OsStr::new("zst")) {
            continue;
        }
        let input = fs::read(&path).unwrap();
        let compressed = compress(&input, level);
        let mut decoded = Vec::new();
        match zstd::stream::copy_decode(compressed.as_slice(), &mut decoded) {
            Ok(()) => {
                if input != decoded {
                    failures.push((path, "input did not equal output".to_owned()));
                }
            }
            Err(e) => failures.push((path, format!("C decoder error: {e:?}"))),
        }
    }
    assert!(
        failures.is_empty(),
        "C decoder roundtrip failed on: {failures:?}"
    );
}

#[test]
fn encode_corpus_files_uncompressed_c_decoder() {
    roundtrip_corpus_through_c_decoder(CompressionLevel::Uncompressed);
}

#[test]
fn encode_corpus_files_compressed_c_decoder() {
    roundtrip_corpus_through_c_decoder(CompressionLevel::Fastest);
}
