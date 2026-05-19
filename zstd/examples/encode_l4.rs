//! Compress an arbitrary file with our encoder at `Level(4)` (greedy).
//! Used by the profile pipeline to produce a `rust_stream` fixture
//! that the `profile_decode` example can iterate over.
//!
//! Usage: `cargo run --release --example encode_l4 <input> <output>`

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input = args.get(1).expect("usage: encode_l4 <input> <output.zst>");
    let output = args.get(2).expect("usage: encode_l4 <input> <output.zst>");
    let data = std::fs::read(input).expect("read input");
    let out = structured_zstd::encoding::compress_to_vec(
        data.as_slice(),
        structured_zstd::encoding::CompressionLevel::Level(4),
    );
    eprintln!("orig {} -> {}", data.len(), out.len());
    std::fs::write(output, &out).expect("write output");
}
