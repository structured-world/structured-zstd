//! The copy a raw block makes, by several methods, over the frames a bench
//! cell cycles through: each frame's payload (past its 14-byte header) into an
//! output placed at several offsets of a page. Separates how a CPU copies from
//! where the buffers sit.
//!
//! Run: `raw_copy_variants <len> <iters> <prefix> <count>`, reading
//! `<prefix>.<i>.zst` for `i` in `0..count`; frames shorter than
//! `14 + len` are skipped.

use std::env;
use std::time::Instant;

type Copy = unsafe fn(*mut u8, *const u8, usize);

unsafe fn by_memcpy(dst: *mut u8, src: *const u8, n: usize) {
    unsafe { core::ptr::copy_nonoverlapping(src, dst, n) }
}

unsafe fn by_memmove(dst: *mut u8, src: *const u8, n: usize) {
    unsafe { core::ptr::copy(src, dst, n) }
}

#[cfg(target_arch = "x86")]
unsafe fn by_rep_movsb(dst: *mut u8, src: *const u8, n: usize) {
    unsafe {
        core::arch::asm!("rep movsb", inout("ecx") n => _, inout("esi") src => _,
            inout("edi") dst => _, options(nostack, preserves_flags));
    }
}

#[cfg(target_arch = "x86_64")]
unsafe fn by_rep_movsb(dst: *mut u8, src: *const u8, n: usize) {
    unsafe {
        core::arch::asm!("rep movsb", inout("rcx") n => _, inout("rsi") src => _,
            inout("rdi") dst => _, options(nostack, preserves_flags));
    }
}

/// 16-byte unaligned loads, stores aligned to 16 after one unaligned head.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
unsafe fn by_sse2_dst_aligned(dst: *mut u8, src: *const u8, n: usize) {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::*;
    unsafe {
        _mm_storeu_si128(dst.cast(), _mm_loadu_si128(src.cast()));
        let head = 16 - (dst as usize & 15);
        let mut i = head;
        while i + 64 <= n {
            let a = _mm_loadu_si128(src.add(i).cast());
            let b = _mm_loadu_si128(src.add(i + 16).cast());
            let c = _mm_loadu_si128(src.add(i + 32).cast());
            let d = _mm_loadu_si128(src.add(i + 48).cast());
            _mm_store_si128(dst.add(i).cast(), a);
            _mm_store_si128(dst.add(i + 16).cast(), b);
            _mm_store_si128(dst.add(i + 32).cast(), c);
            _mm_store_si128(dst.add(i + 48).cast(), d);
            i += 64;
        }
        while i + 16 <= n {
            _mm_store_si128(dst.add(i).cast(), _mm_loadu_si128(src.add(i).cast()));
            i += 16;
        }
        _mm_storeu_si128(
            dst.add(n - 16).cast(),
            _mm_loadu_si128(src.add(n - 16).cast()),
        );
    }
}

/// 32-byte unaligned loads and stores, no alignment.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn by_avx2_unaligned(dst: *mut u8, src: *const u8, n: usize) {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::*;
    unsafe {
        let mut i = 0;
        while i + 64 <= n {
            let a = _mm256_loadu_si256(src.add(i).cast());
            let b = _mm256_loadu_si256(src.add(i + 32).cast());
            _mm256_storeu_si256(dst.add(i).cast(), a);
            _mm256_storeu_si256(dst.add(i + 32).cast(), b);
            i += 64;
        }
        while i + 32 <= n {
            _mm256_storeu_si256(dst.add(i).cast(), _mm256_loadu_si256(src.add(i).cast()));
            i += 32;
        }
        _mm256_storeu_si256(
            dst.add(n - 32).cast(),
            _mm256_loadu_si256(src.add(n - 32).cast()),
        );
    }
}

/// 32-byte unaligned loads, stores aligned to 32 after one unaligned head.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn by_avx2_dst_aligned(dst: *mut u8, src: *const u8, n: usize) {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::*;
    unsafe {
        _mm256_storeu_si256(dst.cast(), _mm256_loadu_si256(src.cast()));
        let mut i = 32 - (dst as usize & 31);
        while i + 64 <= n {
            let a = _mm256_loadu_si256(src.add(i).cast());
            let b = _mm256_loadu_si256(src.add(i + 32).cast());
            _mm256_store_si256(dst.add(i).cast(), a);
            _mm256_store_si256(dst.add(i + 32).cast(), b);
            i += 64;
        }
        while i + 32 <= n {
            _mm256_store_si256(dst.add(i).cast(), _mm256_loadu_si256(src.add(i).cast()));
            i += 32;
        }
        _mm256_storeu_si256(
            dst.add(n - 32).cast(),
            _mm256_loadu_si256(src.add(n - 32).cast()),
        );
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let len: usize = args[1].parse().expect("len");
    let iters: u32 = args[2].parse().expect("iters");
    let prefix = &args[3];
    let count: usize = args[4].parse().expect("count");
    let frames: Vec<Vec<u8>> = (0..count)
        .map(|i| std::fs::read(format!("{prefix}.{i}.zst")).expect("read frame"))
        .filter(|f| f.len() >= 14 + len)
        .collect();
    let mut mods = [0usize; 4];
    for f in &frames {
        mods[(f.as_ptr() as usize + 14) % 64 / 16] += 1;
    }
    println!(
        "{} frames; source mod 64 in [0,16,32,48) buckets: {mods:?}",
        frames.len()
    );

    let srcs: Vec<String> = frames
        .iter()
        .take(4)
        .map(|f| format!("{:#x}", (f.as_ptr() as usize + 14) % 4096))
        .collect();
    println!("first sources mod 4096: {}", srcs.join(" "));
    #[cfg_attr(
        not(any(target_arch = "x86", target_arch = "x86_64")),
        allow(unused_mut)
    )]
    let mut variants: Vec<(&str, Copy)> = vec![("memcpy", by_memcpy), ("memmove", by_memmove)];
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        variants.push(("rep_movsb", by_rep_movsb));
        variants.push(("sse2_dst_al", by_sse2_dst_aligned));
        if std::is_x86_feature_detected!("avx2") {
            variants.push(("avx2_unal", by_avx2_unaligned));
            variants.push(("avx2_dst_al", by_avx2_dst_aligned));
        }
    }

    let mut arena = vec![0u8; len + 2 * 4096];
    let page = (4096 - arena.as_ptr() as usize % 4096) % 4096;
    print!("offset");
    for (name, _) in &variants {
        print!(" {name:>11}");
    }
    println!();
    for step in 0..16 {
        let off = page + step * 256;
        print!("{:6}", step * 256);
        for (_, copy) in &variants {
            let mut best = f64::MAX;
            for _ in 0..5 {
                let mut call = 0usize;
                let t = Instant::now();
                for _ in 0..iters {
                    let f = &frames[call % frames.len()];
                    call += 1;
                    // SAFETY: `f` holds `14 + len` bytes and the arena `off + len`.
                    unsafe {
                        copy(
                            arena.as_mut_ptr().add(off),
                            core::hint::black_box(f.as_ptr().add(14)),
                            len,
                        )
                    };
                    core::hint::black_box(&arena[off]);
                }
                best = best.min(t.elapsed().as_nanos() as f64 / f64::from(iters));
            }
            print!(" {best:11.1}");
        }
        println!();
    }
}
