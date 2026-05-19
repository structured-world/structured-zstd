fn main() {
    let data = std::fs::read(
        "/Users/polaz/.codex/worktrees/a4fa/structured-zstd/zstd/decodecorpus_files/z000033",
    )
    .unwrap();
    let out = structured_zstd::encoding::compress_to_vec(
        data.as_slice(),
        structured_zstd::encoding::CompressionLevel::Level(4),
    );
    eprintln!("orig {} -> {}", data.len(), out.len());
    std::fs::write("/tmp/z033-rust-l4.zst", &out).unwrap();
}
