use std::io::Read;
use structured_zstd::decoding::StreamingDecoder;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .get(1)
        .expect("usage: profile_decode <zst-file> [iters]");
    let iters: u32 = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(10_000);
    let data = std::fs::read(path).expect("read input");
    eprintln!("input {} bytes, iters {}", data.len(), iters);
    let mut total = 0usize;
    for _ in 0..iters {
        let mut decoder = StreamingDecoder::new(data.as_slice()).expect("ctor");
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).expect("decode");
        total = total.wrapping_add(out.len());
    }
    eprintln!("total decoded: {}", total);
}
