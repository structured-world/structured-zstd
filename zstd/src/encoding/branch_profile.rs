//! Per-branch outcome counters for the match-finder's scan paths, compiled
//! only with `kernel-trace`.
//!
//! A branch whose outcome keeps changing from one execution to the next is
//! the one a predictor misses, and the counters record exactly that: how
//! often each probed branch ran, how often it was taken, and how often its
//! outcome differed from its previous one. The counts follow the algorithm,
//! not the machine, so they can be read on any host, including one whose
//! performance counters cannot attribute a miss to a branch.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Branches a scan path may probe.
pub const SLOTS: usize = 32;

static RUNS: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
static TAKEN: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
static FLIPS: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
static LAST: [AtomicBool; SLOTS] = [const { AtomicBool::new(false) }; SLOTS];

/// Record that branch `id` ran with outcome `taken`, and hand `taken` back.
#[inline(always)]
pub fn hit(id: usize, taken: bool) -> bool {
    RUNS[id].fetch_add(1, Ordering::Relaxed);
    if taken {
        TAKEN[id].fetch_add(1, Ordering::Relaxed);
    }
    if LAST[id].swap(taken, Ordering::Relaxed) != taken {
        FLIPS[id].fetch_add(1, Ordering::Relaxed);
    }
    taken
}

/// One line per branch that ran: id, runs, taken and flips.
pub fn report() -> alloc::string::String {
    use core::fmt::Write;
    let mut out = alloc::string::String::new();
    for id in 0..SLOTS {
        let runs = RUNS[id].load(Ordering::Relaxed);
        if runs == 0 {
            continue;
        }
        let taken = TAKEN[id].load(Ordering::Relaxed);
        let flips = FLIPS[id].load(Ordering::Relaxed);
        writeln!(
            out,
            "branch {id:2} runs {runs:>12} taken {taken:>12} flips {flips:>12}"
        )
        .expect("writing to a String does not fail");
    }
    out
}
