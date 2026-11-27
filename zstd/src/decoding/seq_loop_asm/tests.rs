//! Counts the assembly loop's exits, so the per-tier decode tests in
//! `decoding::tier_tests` can tell the loop ran rather than infer it from the
//! CPU tier and the entry threshold.

use crate::cpu_kernel::CpuKernelTag;

std::thread_local! {
    /// Passes of the loop on this thread that ended `Done`, `Boundary` and
    /// `Pending`.
    static EXITS: core::cell::Cell<[u64; 3]> = const { core::cell::Cell::new([0; 3]) };
    /// The entry form the loop takes on this thread in place of the CPU's
    /// (`true` for the words).
    static FORM: core::cell::Cell<Option<bool>> = const { core::cell::Cell::new(None) };
}

/// The entry form a test forced on this thread, if any (`true` for words).
pub(in crate::decoding) fn forced_words() -> Option<bool> {
    FORM.with(core::cell::Cell::get)
}

/// Runs `f` once per entry form for the AVX2 kernel, the one whose loop has
/// both, with the loop forced to it (named in `f`'s argument); once for any
/// other kernel. Whichever form the CPU would choose, both are decoded.
pub(in crate::decoding) fn for_each_entry_form(kernel: CpuKernelTag, mut f: impl FnMut(&str)) {
    if kernel != CpuKernelTag::Avx2 {
        return f("-");
    }
    for (form, name) in [(false, "pointers"), (true, "words")] {
        FORM.with(|c| c.set(Some(form)));
        f(name);
    }
    FORM.with(|c| c.set(None));
}

std::thread_local! {
    /// Experiment: blocks seen by the AVX2 short arm, their sequences in all,
    /// and those of blocks with fewer than the loop's entry threshold.
    static BLOCKS: core::cell::Cell<[u64; 4]> = const { core::cell::Cell::new([0; 4]) };
}

/// Experiment: counts a block of `n` sequences on the AVX2 short arm.
pub(in crate::decoding) fn count_block(n: usize) {
    BLOCKS.with(|b| {
        let mut c = b.get();
        c[0] += 1;
        c[1] += n as u64;
        if n <= super::MIN_SEQUENCES {
            c[2] += 1;
            c[3] += n as u64;
        }
        b.set(c);
    });
}

/// Counts one pass by the assembly's status (0 done, 1 boundary, 2 pending).
pub(super) fn count_exit(status: usize) {
    EXITS.with(|exits| {
        let mut counts = exits.get();
        counts[status.min(2)] += 1;
        exits.set(counts);
    });
}

/// The exit counts on this thread so far.
pub(in crate::decoding) fn exits() -> [u64; 3] {
    EXITS.with(|exits| exits.get())
}

/// Asserts that since `before` the loop stopped at the stream's start and
/// handed sequences to Rust. `Done` is not asked for: a block's last
/// sequences sit in the first bytes of its bit stream, below the floor, so an
/// entered block ends at `Boundary` unless the stream outlasts its sequences,
/// which no frame of those tests produces.
pub(in crate::decoding) fn assert_loop_ran_since(before: [u64; 3], shape: usize) {
    let after = exits();
    for (exit, name) in [(1, "Boundary"), (2, "Pending")] {
        assert!(
            after[exit] > before[exit],
            "shape {shape}: the assembly loop never exited {name} \
             (exits before {before:?}, after {after:?})"
        );
    }
}

/// Experiment: the loop's exits over a frame list named by `LSM_FRAMES`
/// (`[u32 len][u32 decoded][frame]...`); skipped without it.
#[test]
fn exits_over_a_frame_list() {
    let Ok(path) = std::env::var("LSM_FRAMES") else {
        return;
    };
    let data = std::fs::read(path).unwrap();
    let mut decoder = crate::decoding::FrameDecoder::new();
    let mut at = 0usize;
    let mut frames = 0u64;
    let before = exits();
    while at < data.len() {
        let len = u32::from_le_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        let decoded = u32::from_le_bytes(data[at + 4..at + 8].try_into().unwrap()) as usize;
        at += 8;
        let mut out = std::vec![0u8; decoded];
        assert_eq!(
            decoder.decode_all(&data[at..at + len], &mut out).unwrap(),
            decoded
        );
        at += len;
        frames += 1;
    }
    let after = exits();
    let blocks = BLOCKS.with(core::cell::Cell::get);
    std::eprintln!(
        "frames {frames} done {} boundary {} pending {} | short-arm blocks {} sequences {} | below entry: blocks {} sequences {}",
        after[0] - before[0],
        after[1] - before[1],
        after[2] - before[2],
        blocks[0],
        blocks[1],
        blocks[2],
        blocks[3]
    );
}

/// Whether the kernels include AVX2, the one whose loop is the assembly; says
/// so when they do not, so a host that skips the loop's checks shows it.
pub(in crate::decoding) fn assembly_runs(kernels: &[CpuKernelTag]) -> bool {
    let runs = kernels.contains(&CpuKernelTag::Avx2);
    if !runs {
        std::eprintln!("no AVX2 kernel on this CPU: the assembly loop is not exercised");
    }
    runs
}
