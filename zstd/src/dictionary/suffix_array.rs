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
/// If `text` is `u32::MAX` bytes or longer, since positions are held as `u32`
/// with one value kept free as the empty mark.
pub(crate) fn suffix_array(text: &[u8]) -> Vec<u32> {
    assert!(
        text.len() < EMPTY as usize,
        "a suffix array of {} bytes does not fit u32 positions",
        text.len()
    );
    sa_is(text, usize::from(u8::MAX))
}

/// SA-IS over `s`, whose symbols all lie in `0..=upper`. The text is taken to
/// end in a virtual sentinel smaller than every symbol.
fn sa_is<T: Symbol>(s: &[T], upper: usize) -> Vec<u32> {
    let n = s.len();
    match n {
        0 => return Vec::new(),
        1 => return vec![0],
        2 => return if s[0] < s[1] { vec![0, 1] } else { vec![1, 0] },
        _ => {}
    }
    if n < NAIVE_THRESHOLD {
        let mut sa: Vec<u32> = (0..n as u32).collect();
        sa.sort_unstable_by(|&a, &b| s[a as usize..].cmp(&s[b as usize..]));
        return sa;
    }

    // `ls[i]`: the suffix at `i` is S-type (smaller than the one after it).
    // The last is L-type against the virtual sentinel.
    let mut ls = vec![false; n];
    for i in (0..n - 1).rev() {
        ls[i] = if s[i] == s[i + 1] {
            ls[i + 1]
        } else {
            s[i] < s[i + 1]
        };
    }
    // Bucket starts: `sum_l[c]` where the L-type suffixes of `c` begin,
    // `sum_s[c]` where its S-type ones do.
    let mut sum_l = vec![0u32; upper + 1];
    let mut sum_s = vec![0u32; upper + 1];
    for i in 0..n {
        if ls[i] {
            // An S-type suffix is followed by a larger symbol, so its own is
            // never the largest and the next bucket exists.
            debug_assert!(s[i].index() < upper);
            sum_l[s[i].index() + 1] += 1;
        } else {
            sum_s[s[i].index()] += 1;
        }
    }
    for c in 0..=upper {
        sum_s[c] += sum_l[c];
        if c < upper {
            sum_l[c + 1] += sum_s[c];
        }
    }

    let mut sa = vec![EMPTY; n];
    let mut buf = vec![0u32; upper + 1];
    let induce = |sa: &mut [u32], buf: &mut [u32], lms: &[u32]| {
        sa.fill(EMPTY);
        buf.copy_from_slice(&sum_s);
        for &d in lms {
            let d = d as usize;
            if d == n {
                continue;
            }
            let c = s[d].index();
            sa[buf[c] as usize] = d as u32;
            buf[c] += 1;
        }
        buf.copy_from_slice(&sum_l);
        let c = s[n - 1].index();
        sa[buf[c] as usize] = (n - 1) as u32;
        buf[c] += 1;
        // The two induction sweeps are the whole cost of the construction, one
        // random write per suffix. A slot holding the empty mark or position 0
        // has no predecessor to induce, and one unsigned compare of `v - 1`
        // against `n` rejects both. Every index below is in bounds by the
        // bucket layout: a position is below `n`, and each bucket's cursor
        // stays inside the bucket its symbol's suffixes fill.
        for i in 0..n {
            // SAFETY: `i < n == sa.len()`.
            let p = unsafe { *sa.get_unchecked(i) }.wrapping_sub(1) as usize;
            if p < n {
                // SAFETY: `p < n == ls.len() == s.len()`.
                if !unsafe { *ls.get_unchecked(p) } {
                    let c = unsafe { s.get_unchecked(p) }.index();
                    debug_assert!(c < buf.len() && (buf[c] as usize) < n);
                    // SAFETY: `c <= upper` (symbols lie in `0..=upper` and
                    // `buf.len() == upper + 1`); `buf[c] < n` by the layout.
                    unsafe {
                        let slot = buf.get_unchecked_mut(c);
                        *sa.get_unchecked_mut(*slot as usize) = p as u32;
                        *slot += 1;
                    }
                }
            }
        }
        buf.copy_from_slice(&sum_l);
        for i in (0..n).rev() {
            // SAFETY: `i < n == sa.len()`.
            let p = unsafe { *sa.get_unchecked(i) }.wrapping_sub(1) as usize;
            if p < n {
                // SAFETY: `p < n == ls.len() == s.len()`.
                if unsafe { *ls.get_unchecked(p) } {
                    let c = unsafe { s.get_unchecked(p) }.index() + 1;
                    debug_assert!(c < buf.len());
                    // SAFETY: an S-type symbol is below `upper`, so
                    // `c <= upper`; the bucket's end cursor is above its start.
                    unsafe {
                        let slot = buf.get_unchecked_mut(c);
                        *slot -= 1;
                        debug_assert!((*slot as usize) < n);
                        *sa.get_unchecked_mut(*slot as usize) = p as u32;
                    }
                }
            }
        }
    };

    // The leftmost S-type positions, and each one's rank among them.
    let mut lms_map = vec![EMPTY; n + 1];
    let mut lms = Vec::new();
    for i in 1..n {
        if !ls[i - 1] && ls[i] {
            lms_map[i] = lms.len() as u32;
            lms.push(i as u32);
        }
    }
    let m = lms.len();

    induce(&mut sa, &mut buf, &lms);

    if m > 0 {
        let mut sorted_lms: Vec<u32> = sa
            .iter()
            .copied()
            .filter(|&v| v != EMPTY && lms_map[v as usize] != EMPTY)
            .collect();
        // Name each LMS substring by rank, equal substrings sharing a name, and
        // sort the string of names recursively.
        let mut rec_s = vec![0u32; m];
        let mut rec_upper = 0u32;
        rec_s[lms_map[sorted_lms[0] as usize] as usize] = 0;
        for i in 1..m {
            let mut l = sorted_lms[i - 1] as usize;
            let mut r = sorted_lms[i] as usize;
            let next = |p: usize| {
                let rank = lms_map[p] as usize;
                if rank + 1 < m {
                    lms[rank + 1] as usize
                } else {
                    n
                }
            };
            let end_l = next(l);
            let end_r = next(r);
            let mut same = true;
            if end_l - l != end_r - r {
                same = false;
            } else {
                while l < end_l {
                    if s[l] != s[r] {
                        break;
                    }
                    l += 1;
                    r += 1;
                }
                if l == n || s[l] != s[r] {
                    same = false;
                }
            }
            if !same {
                rec_upper += 1;
            }
            rec_s[lms_map[sorted_lms[i] as usize] as usize] = rec_upper;
        }

        let rec_sa = sa_is(&rec_s, rec_upper as usize);
        for (slot, &rank) in sorted_lms.iter_mut().zip(&rec_sa) {
            *slot = lms[rank as usize];
        }
        induce(&mut sa, &mut buf, &sorted_lms);
    }
    sa
}

#[cfg(test)]
mod tests;
