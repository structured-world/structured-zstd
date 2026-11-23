//! Counts the assembly loop's exits, so the per-tier decode tests in
//! `decoding::tier_tests` can tell the loop ran rather than infer it from the
//! CPU tier and the entry threshold.

use crate::cpu_kernel::CpuKernelTag;

std::thread_local! {
    /// Passes of the loop on this thread that ended `Done`, `Boundary` and
    /// `Pending`.
    static EXITS: core::cell::Cell<[u64; 3]> = const { core::cell::Cell::new([0; 3]) };
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

/// Whether the kernels include AVX2, the one whose loop is the assembly; says
/// so when they do not, so a host that skips the loop's checks shows it.
pub(in crate::decoding) fn assembly_runs(kernels: &[CpuKernelTag]) -> bool {
    let runs = kernels.contains(&CpuKernelTag::Avx2);
    if !runs {
        std::eprintln!("no AVX2 kernel on this CPU: the assembly loop is not exercised");
    }
    runs
}
