//! Parity test: our `ZSTD_getCParams` port (`structured_zstd::testing::c_cparams`)
//! must match the C `ZSTD_getCParams` byte-for-byte across a grid of
//! (level, srcSize, dictSize). This locks the cparam tier table + adjust logic
//! to the reference so the encoder sizes its tables and picks its strategy the
//! same way upstream does.
#![cfg(feature = "bench-internals")]
use zstd::zstd_safe::zstd_sys;

fn reference_cparams(level: i32, src: u64, dict: usize) -> (u32, u32, u32, u32, u32, u32, u32) {
    let p = unsafe { zstd_sys::ZSTD_getCParams(level, src, dict) };
    (
        p.windowLog,
        p.chainLog,
        p.hashLog,
        p.searchLog,
        p.minMatch,
        p.targetLength,
        p.strategy as u32,
    )
}

#[test]
fn cparams_match_reference_over_grid() {
    let levels = [-7, -5, -3, -1, 0, 1, 2, 3, 4, 5, 7, 10, 12, 15, 17, 19, 22];
    let srcs: [u64; 9] = [0, 1, 100, 4096, 6806, 16_384, 100_000, 131_072, 5_000_000];
    let dicts: [usize; 5] = [0, 437, 2048, 65_536, 1_000_000];
    let mut mismatches = 0usize;
    for &level in &levels {
        for &src in &srcs {
            for &dict in &dicts {
                let ours = structured_zstd::testing::compression_params(level, src, dict);
                let reference = reference_cparams(level, src, dict);
                if ours != reference {
                    mismatches += 1;
                    if mismatches <= 40 {
                        eprintln!(
                            "MISMATCH level={level} src={src} dict={dict}\n  ours={ours:?}\n  ref ={reference:?}"
                        );
                    }
                }
            }
        }
    }
    assert_eq!(
        mismatches, 0,
        "{mismatches} cparam mismatches vs C ZSTD_getCParams"
    );
}
