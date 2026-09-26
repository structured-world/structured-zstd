//! Suffix-array construction by induced sorting (SA-IS, Nong, Zhang and Chan,
//! "Two Efficient Algorithms for Linear Time Suffix Array Construction").
//!
//! A suffix array is unique for its text: the positions of every suffix in
//! lexicographic order, a suffix that is a prefix of another ordered first.
//! So any correct construction yields the array the reference trainer gets
//! from `divsufsort`, which is what makes the legacy trainer built on it
//! reproduce the reference's choices.

use alloc::vec;
use alloc::vec::Vec;

/// Marks an empty slot of the array under construction.
const EMPTY: u32 = u32::MAX;

/// Below this length a comparison sort is cheaper than induction.
const NAIVE_THRESHOLD: usize = 10;

/// A symbol of the text being sorted: the bytes at the top level, the ranks of
/// the reduced string in the recursion.
trait Symbol: Copy + Ord {
    fn index(self) -> usize;
}

impl Symbol for u8 {
    #[inline]
    fn index(self) -> usize {
        usize::from(self)
    }
}

impl Symbol for u32 {
    #[inline]
    fn index(self) -> usize {
        self as usize
    }
}

/// The suffix array of `text`: `sa[i]` is the start of the `i`-th smallest
/// suffix.
///
/// # Panics
///
/// If `text` is 2 GiB or longer: positions are held as `u32` whose top bit
/// marks an entry during construction.
pub(crate) fn suffix_array(text: &[u8]) -> Vec<u32> {
    assert!(
        text.len() < 1 << 31,
        "a suffix array of {} bytes does not fit 31-bit positions",
        text.len()
    );
    let mut sa = vec![0u32; text.len()];
    sa_is(text, &mut sa, usize::from(u8::MAX));
    sa
}

/// Whether each suffix is S-type (smaller than the one after it), a bit per
/// position. The last is L-type against the virtual sentinel.
struct Types {
    words: Vec<u64>,
}

impl Types {
    fn new<T: Symbol>(s: &[T]) -> Self {
        let n = s.len();
        let mut words = vec![0u64; n.div_ceil(64)];
        let mut next = false;
        for i in (0..n - 1).rev() {
            let small = if s[i] == s[i + 1] {
                next
            } else {
                s[i] < s[i + 1]
            };
            words[i / 64] |= u64::from(small) << (i % 64);
            next = small;
        }
        Self { words }
    }

    #[inline]
    fn is_s(&self, i: usize) -> bool {
        (self.words[i / 64] >> (i % 64)) & 1 != 0
    }

    /// Whether `p` starts a leftmost S-type run.
    #[inline]
    fn is_lms(&self, p: usize, n: usize) -> bool {
        p > 0 && p < n && self.is_s(p) && !self.is_s(p - 1)
    }
}

/// `buf[c]` = the first slot of symbol `c`'s bucket.
fn bucket_starts(counts: &[u32], buf: &mut [u32]) {
    let mut sum = 0;
    for (slot, &count) in buf.iter_mut().zip(counts) {
        *slot = sum;
        sum += count;
    }
}

/// `buf[c]` = one past the last slot of symbol `c`'s bucket.
fn bucket_ends(counts: &[u32], buf: &mut [u32]) {
    let mut sum = 0;
    for (slot, &count) in buf.iter_mut().zip(counts) {
        sum += count;
        *slot = sum;
    }
}

/// SA-IS over `s` into `sa` (as long as `s`), whose symbols all lie in
/// `0..=upper`. The text is taken to end in a virtual sentinel smaller than
/// every symbol.
///
/// Laid out as Yuta Mori's `sais.c` lays it out, with nothing beside `sa` but
/// the bucket counters and a bit per position: the reduced string is gathered
/// at the back of `sa` and sorted recursively into the front, so no level
/// holds a copy of its input or of the array.
fn sa_is<T: Symbol>(s: &[T], sa: &mut [u32], upper: usize) {
    let n = s.len();
    debug_assert_eq!(sa.len(), n);
    match n {
        0 => return,
        1 => {
            sa[0] = 0;
            return;
        }
        2 => {
            sa.copy_from_slice(if s[0] < s[1] { &[0, 1] } else { &[1, 0] });
            return;
        }
        _ => {}
    }
    if n < NAIVE_THRESHOLD {
        for (at, slot) in sa.iter_mut().enumerate() {
            *slot = at as u32;
        }
        sa.sort_unstable_by(|&a, &b| s[a as usize..].cmp(&s[b as usize..]));
        return;
    }

    let types = Types::new(s);
    let mut counts = vec![0u32; upper + 1];
    for &c in s {
        counts[c.index()] += 1;
    }
    let mut buf = vec![0u32; upper + 1];

    // Stage 1: the LMS positions at their buckets' ends, in any order, induce
    // the order of the LMS substrings.
    sa.fill(0);
    bucket_ends(&counts, &mut buf);
    let mut m = 0;
    for p in (1..n).rev() {
        if types.is_lms(p, n) {
            let c = s[p].index();
            buf[c] -= 1;
            sa[buf[c] as usize] = p as u32;
            m += 1;
        }
    }
    induce(s, sa, &counts, &mut buf);
    if m == 0 {
        // No LMS suffix: the induction from the last suffix alone is the
        // whole order.
        return;
    }

    // Name each LMS substring by rank, equal substrings sharing a name. The
    // sorted LMS positions are compacted to the front, each one's name goes
    // to `m + p / 2` (LMS positions are at least two apart, so the slots are
    // distinct and lie past the first `m`), then the names are gathered in
    // text order at the back. A substring runs from its LMS position to the
    // next one, or to the end of the text.
    let mut k = 0;
    for i in 0..n {
        let v = sa[i] as usize;
        if types.is_lms(v, n) {
            sa[k] = v as u32;
            k += 1;
        }
    }
    debug_assert_eq!(k, m);
    sa[m..].fill(EMPTY);
    let substring_end = |p: usize| {
        let mut end = p + 1;
        while end < n && !types.is_lms(end, n) {
            end += 1;
        }
        end
    };
    let mut name = 0u32;
    let mut prev = sa[0] as usize;
    let mut prev_end = substring_end(prev);
    debug_assert!(m + prev / 2 < n);
    sa[m + prev / 2] = 0;
    for i in 1..m {
        let cur = sa[i] as usize;
        let cur_end = substring_end(cur);
        // Equal when as long and equal symbol for symbol, the symbol after
        // included. One that runs into the end of the text ends at the virtual
        // sentinel, which nothing else reaches, so it is unique.
        let same = cur_end - cur == prev_end - prev
            && cur_end < n
            && prev_end < n
            && s[cur..=cur_end] == s[prev..=prev_end];
        if !same {
            name += 1;
        }
        debug_assert!(m + cur / 2 < n);
        sa[m + cur / 2] = name;
        prev = cur;
        prev_end = cur_end;
    }
    let mut j = n;
    for i in (m..n).rev() {
        if sa[i] != EMPTY {
            j -= 1;
            sa[j] = sa[i];
        }
    }
    debug_assert_eq!(j, n - m);

    // Stage 2: sort the reduced string into the front of `sa`. With every
    // name distinct its order is the names themselves, and no recursion is
    // needed.
    {
        // `m <= n / 2`, so the front `m` slots and the back `m` are disjoint.
        let (front, reduced) = sa.split_at_mut(n - m);
        let order = &mut front[..m];
        if name as usize + 1 == m {
            for (at, &rank) in reduced.iter().enumerate() {
                order[rank as usize] = at as u32;
            }
        } else {
            sa_is(&*reduced, order, name as usize);
        }
        // The reduced string is done with: its slots take the LMS positions
        // in text order, which turn the sorted indices into positions.
        let mut at = 0;
        for p in 1..n {
            if types.is_lms(p, n) {
                reduced[at] = p as u32;
                at += 1;
            }
        }
        debug_assert_eq!(at, m);
        for slot in order.iter_mut() {
            *slot = reduced[*slot as usize];
        }
    }

    // Stage 3: the sorted LMS positions to their buckets' ends, keeping their
    // order, from the back so that no entry is overwritten before it is read
    // (`sais.c`, `sais_main` stage 3). A bucket's end is at least the number
    // of LMS positions of its symbol and below, so the write cursor never
    // passes the read one.
    bucket_ends(&counts, &mut buf);
    let mut i = m;
    let mut j = n;
    while i > 0 {
        let mut p = sa[i - 1];
        let c = s[p as usize].index();
        let end = buf[c] as usize;
        while end < j {
            j -= 1;
            sa[j] = 0;
        }
        loop {
            debug_assert!(j >= i);
            j -= 1;
            sa[j] = p;
            i -= 1;
            if i == 0 {
                break;
            }
            p = sa[i - 1];
            if s[p as usize].index() != c {
                break;
            }
        }
    }
    sa[..j].fill(0);
    induce(s, sa, &counts, &mut buf);
}

/// The two induction sweeps from the LMS positions already in `sa` (every
/// other slot zero): the L-type suffixes left to right from the bucket
/// starts, then the S-type ones right to left from the bucket ends.
///
/// The sweeps are the whole cost of the construction: one random write per
/// suffix. They follow Yuta Mori's `sais.c` (`induceSA`): whether a suffix's
/// predecessor is to be induced in the sweep that reads it is decided when the
/// suffix is written, from the symbol before it, which is next to the one just
/// read, and kept as the complement of the position (`!j`, the top bit set).
/// The L sweep complements every entry it reads, which turns exactly the
/// entries whose predecessor is S-type into the live ones for the S sweep; the
/// S sweep restores the rest. The bucket cursor stays in a register while the
/// symbol does not change.
///
/// Every index below is in bounds by the bucket layout: a position is below
/// `n`, a symbol lies in `0..=upper`, and each cursor stays inside the bucket
/// its symbol's suffixes fill.
fn induce<T: Symbol>(s: &[T], sa: &mut [u32], counts: &[u32], buf: &mut [u32]) {
    let n = s.len();
    let live = |v: u32| (v as i32) > 0;

    bucket_starts(counts, buf);
    let last = n - 1;
    let mut c1 = s[last].index();
    let mut b = buf[c1] as usize;
    sa[b] = if s[last - 1] < s[last] {
        !(last as u32)
    } else {
        last as u32
    };
    b += 1;
    for i in 0..n {
        // SAFETY: `i < n == sa.len()`.
        let v = unsafe { *sa.get_unchecked(i) };
        unsafe { *sa.get_unchecked_mut(i) = !v };
        if live(v) {
            let j = v as usize - 1;
            // SAFETY: `j < n == s.len()`.
            let c0 = unsafe { s.get_unchecked(j) }.index();
            if c0 != c1 {
                debug_assert!(c0 < buf.len() && c1 < buf.len());
                // SAFETY: both symbols lie in `0..=upper`.
                unsafe {
                    *buf.get_unchecked_mut(c1) = b as u32;
                    b = *buf.get_unchecked(c0) as usize;
                }
                c1 = c0;
            }
            // `j` is L-type; its predecessor is live for this sweep when it is
            // L-type as well, which here means not smaller.
            let dead = j > 0 && unsafe { s.get_unchecked(j - 1) }.index() < c1;
            debug_assert!(b < n);
            // SAFETY: `b` stays inside bucket `c1`.
            unsafe { *sa.get_unchecked_mut(b) = if dead { !(j as u32) } else { j as u32 } };
            b += 1;
        }
    }

    bucket_ends(counts, buf);
    let mut c1 = 0usize;
    let mut b = buf[0] as usize;
    for i in (0..n).rev() {
        // SAFETY: `i < n == sa.len()`.
        let v = unsafe { *sa.get_unchecked(i) };
        if live(v) {
            let j = v as usize - 1;
            // SAFETY: `j < n == s.len()`.
            let c0 = unsafe { s.get_unchecked(j) }.index();
            if c0 != c1 {
                debug_assert!(c0 < buf.len() && c1 < buf.len());
                // SAFETY: both symbols lie in `0..=upper`.
                unsafe {
                    *buf.get_unchecked_mut(c1) = b as u32;
                    b = *buf.get_unchecked(c0) as usize;
                }
                c1 = c0;
            }
            // `j` is S-type; its predecessor is live when it is S-type as
            // well, which here means not larger.
            let dead = j == 0 || unsafe { s.get_unchecked(j - 1) }.index() > c1;
            debug_assert!(b > 0);
            b -= 1;
            // SAFETY: `b` stays inside bucket `c1`.
            unsafe { *sa.get_unchecked_mut(b) = if dead { !(j as u32) } else { j as u32 } };
        } else {
            unsafe { *sa.get_unchecked_mut(i) = !v };
        }
    }
}

#[cfg(test)]
mod tests;
