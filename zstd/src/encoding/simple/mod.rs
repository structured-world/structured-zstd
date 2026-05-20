//! Simple ("fastest") match finder used by the `Fastest` /
//! `CompressionLevel::Level(1)` backend.
//!
//! Donor parity: tracks the same level-1 fast-pass strategy as
//! `ZSTD_fast.c` but uses an internal suffix-store keyed by a 5-byte
//! polynomial hash. The matcher iterates the current block one suffix
//! at a time, probes every window entry, and emits either a `Triple`
//! sequence (literals + back-reference) or a trailing `Literals`
//! sequence when the remaining bytes cannot beat `MIN_MATCH_LEN`.
//!
//! Extracted from `match_generator.rs` as part of #111 Phase 1c
//! (structural split). Mechanical move — names, fields, method bodies,
//! and the `#[inline(always)]` annotations are preserved; the
//! visibility on the relocated items was opened to `pub(crate)` so
//! `match_generator` can keep dispatching to `MatchGenerator` through
//! the `simple::` import path.

use alloc::vec::Vec;
use core::num::NonZeroUsize;

use super::Sequence;
use super::blocks::encode_offset_with_history;
use super::match_table::helpers::{
    FAST_HASH_FILL_STEP, INCOMPRESSIBLE_SKIP_STEP, MIN_MATCH_LEN, common_prefix_len,
};

pub(crate) mod fast_kernel;

/// This stores the index of a suffix of a string by hashing the first few bytes of that suffix
/// This means that collisions just overwrite and that you need to check validity after a get
pub(crate) struct SuffixStore {
    // We use NonZeroUsize to enable niche optimization here.
    // On store we do +1 and on get -1
    // This is ok since usize::MAX is never a valid offset
    pub(crate) slots: Vec<Option<NonZeroUsize>>,
    pub(crate) len_log: u32,
}

impl SuffixStore {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            slots: alloc::vec![None; capacity],
            len_log: capacity.ilog2(),
        }
    }

    #[inline(always)]
    pub(crate) fn insert(&mut self, suffix: &[u8], idx: usize) {
        let key = self.key(suffix);
        self.slots[key] = Some(NonZeroUsize::new(idx + 1).unwrap());
    }

    #[inline(always)]
    pub(crate) fn contains_key(&self, suffix: &[u8]) -> bool {
        let key = self.key(suffix);
        self.slots[key].is_some()
    }

    #[inline(always)]
    pub(crate) fn get(&self, suffix: &[u8]) -> Option<usize> {
        let key = self.key(suffix);
        self.slots[key].map(|x| <NonZeroUsize as Into<usize>>::into(x) - 1)
    }

    #[inline(always)]
    fn key(&self, suffix: &[u8]) -> usize {
        // Capacity=1 yields len_log=0; shifting by 64 would panic.
        if self.len_log == 0 {
            return 0;
        }

        let s0 = suffix[0] as u64;
        let s1 = suffix[1] as u64;
        let s2 = suffix[2] as u64;
        let s3 = suffix[3] as u64;
        let s4 = suffix[4] as u64;

        const POLY: u64 = 0xCF3BCCDCABu64;

        let s0 = (s0 << 24).wrapping_mul(POLY);
        let s1 = (s1 << 32).wrapping_mul(POLY);
        let s2 = (s2 << 40).wrapping_mul(POLY);
        let s3 = (s3 << 48).wrapping_mul(POLY);
        let s4 = (s4 << 56).wrapping_mul(POLY);

        let index = s0 ^ s1 ^ s2 ^ s3 ^ s4;
        let index = index >> (64 - self.len_log);
        index as usize % self.slots.len()
    }
}

/// We keep a window of a few of these entries
/// All of these are valid targets for a match to be generated for
pub(crate) struct WindowEntry {
    pub(crate) data: Vec<u8>,
    /// Stores indexes into data
    pub(crate) suffixes: SuffixStore,
    /// Makes offset calculations efficient
    pub(crate) base_offset: usize,
}

pub(crate) struct MatchGenerator {
    pub(crate) max_window_size: usize,
    /// Data window we are operating on to find matches
    /// The data we want to find matches for is in the last slice
    pub(crate) window: Vec<WindowEntry>,
    pub(crate) window_size: usize,
    #[cfg(debug_assertions)]
    pub(crate) concat_window: Vec<u8>,
    /// Index in the last slice that we already processed
    pub(crate) suffix_idx: usize,
    /// Gets updated when a new sequence is returned to point right behind that sequence
    pub(crate) last_idx_in_sequence: usize,
    pub(crate) hash_fill_step: usize,
    pub(crate) offset_hist: [u32; 3],
}

impl MatchGenerator {
    /// max_size defines how many bytes will be used at most in the window used for matching
    pub(crate) fn new(max_size: usize) -> Self {
        Self {
            max_window_size: max_size,
            window: Vec::new(),
            window_size: 0,
            #[cfg(debug_assertions)]
            concat_window: Vec::new(),
            suffix_idx: 0,
            last_idx_in_sequence: 0,
            hash_fill_step: 1,
            offset_hist: [1, 4, 8],
        }
    }

    pub(crate) fn reset(&mut self, mut reuse_space: impl FnMut(Vec<u8>, SuffixStore)) {
        self.window_size = 0;
        #[cfg(debug_assertions)]
        self.concat_window.clear();
        self.suffix_idx = 0;
        self.last_idx_in_sequence = 0;
        self.offset_hist = [1, 4, 8];
        self.window.drain(..).for_each(|entry| {
            reuse_space(entry.data, entry.suffixes);
        });
    }

    /// Processes bytes in the current window until either a match is found or no more matches can be found
    /// * If a match is found handle_sequence is called with the Triple variant
    /// * If no more matches can be found but there are bytes still left handle_sequence is called with the Literals variant
    /// * If no more matches can be found and no more bytes are left this returns false
    pub(crate) fn next_sequence(
        &mut self,
        mut handle_sequence: impl for<'a> FnMut(Sequence<'a>),
    ) -> bool {
        loop {
            let last_entry = self.window.last().unwrap();
            let data_slice = &last_entry.data;

            // We already reached the end of the window, check if we need to return a Literals{}
            if self.suffix_idx >= data_slice.len() {
                if self.last_idx_in_sequence != self.suffix_idx {
                    let literals = &data_slice[self.last_idx_in_sequence..];
                    self.last_idx_in_sequence = self.suffix_idx;
                    handle_sequence(Sequence::Literals { literals });
                    return true;
                } else {
                    return false;
                }
            }

            // If the remaining data is smaller than the minimum match length we can stop and return a Literals{}
            let data_slice = &data_slice[self.suffix_idx..];
            if data_slice.len() < MIN_MATCH_LEN {
                let last_idx_in_sequence = self.last_idx_in_sequence;
                self.last_idx_in_sequence = last_entry.data.len();
                self.suffix_idx = last_entry.data.len();
                handle_sequence(Sequence::Literals {
                    literals: &last_entry.data[last_idx_in_sequence..],
                });
                return true;
            }

            // This is the key we are looking to find a match for
            let key = &data_slice[..MIN_MATCH_LEN];
            let literals_len = self.suffix_idx - self.last_idx_in_sequence;

            // Look in each window entry
            let mut candidate = self.repcode_candidate(data_slice, literals_len);
            for match_entry in self.window.iter() {
                if let Some(match_index) = match_entry.suffixes.get(key) {
                    let match_slice = &match_entry.data[match_index..];

                    // Check how long the common prefix actually is
                    let match_len = common_prefix_len(match_slice, data_slice);

                    // Collisions in the suffix store might make this check fail
                    if match_len >= MIN_MATCH_LEN {
                        let offset = match_entry.base_offset + self.suffix_idx - match_index;

                        // If we are in debug/tests make sure the match we found is actually at the offset we calculated
                        #[cfg(debug_assertions)]
                        {
                            let unprocessed = last_entry.data.len() - self.suffix_idx;
                            let start = self.concat_window.len() - unprocessed - offset;
                            let end = start + match_len;
                            let check_slice = &self.concat_window[start..end];
                            debug_assert_eq!(check_slice, &match_slice[..match_len]);
                        }

                        if let Some((old_offset, old_match_len)) = candidate {
                            if match_len > old_match_len
                                || (match_len == old_match_len && offset < old_offset)
                            {
                                candidate = Some((offset, match_len));
                            }
                        } else {
                            candidate = Some((offset, match_len));
                        }
                    }
                }
            }

            if let Some((offset, match_len)) = candidate {
                // For each index in the match we found we do not need to look for another match
                // But we still want them registered in the suffix store
                self.add_suffixes_till(self.suffix_idx + match_len, self.hash_fill_step);

                // All literals that were not included between this match and the last are now included here
                let last_entry = self.window.last().unwrap();
                let literals = &last_entry.data[self.last_idx_in_sequence..self.suffix_idx];

                // Update the indexes, all indexes upto and including the current index have been included in a sequence now
                self.suffix_idx += match_len;
                self.last_idx_in_sequence = self.suffix_idx;
                let _ = encode_offset_with_history(
                    offset as u32,
                    literals.len() as u32,
                    &mut self.offset_hist,
                );
                handle_sequence(Sequence::Triple {
                    literals,
                    offset,
                    match_len,
                });

                return true;
            }

            let last_entry = self.window.last_mut().unwrap();
            let key = &last_entry.data[self.suffix_idx..self.suffix_idx + MIN_MATCH_LEN];
            if !last_entry.suffixes.contains_key(key) {
                last_entry.suffixes.insert(key, self.suffix_idx);
            }
            self.suffix_idx += 1;
        }
    }

    /// Process bytes and add the suffixes to the suffix store up to a specific index
    #[inline(always)]
    pub(crate) fn add_suffixes_till(&mut self, idx: usize, fill_step: usize) {
        let start = self.suffix_idx;
        let last_entry = self.window.last_mut().unwrap();
        if last_entry.data.len() < MIN_MATCH_LEN {
            return;
        }
        let insert_limit = idx.saturating_sub(MIN_MATCH_LEN - 1);
        if insert_limit > start {
            let data = last_entry.data.as_slice();
            let suffixes = &mut last_entry.suffixes;
            if fill_step == FAST_HASH_FILL_STEP {
                Self::add_suffixes_interleaved_fast(data, suffixes, start, insert_limit);
            } else {
                let mut pos = start;
                while pos < insert_limit {
                    Self::insert_suffix_if_absent(data, suffixes, pos);
                    pos += fill_step;
                }
            }
        }

        if idx >= start + MIN_MATCH_LEN {
            let tail_start = idx - MIN_MATCH_LEN;
            let tail_key = &last_entry.data[tail_start..tail_start + MIN_MATCH_LEN];
            if !last_entry.suffixes.contains_key(tail_key) {
                last_entry.suffixes.insert(tail_key, tail_start);
            }
        }
    }

    #[inline(always)]
    fn insert_suffix_if_absent(data: &[u8], suffixes: &mut SuffixStore, pos: usize) {
        debug_assert!(
            pos + MIN_MATCH_LEN <= data.len(),
            "insert_suffix_if_absent: pos {} + MIN_MATCH_LEN {} exceeds data.len() {}",
            pos,
            MIN_MATCH_LEN,
            data.len()
        );
        let key = &data[pos..pos + MIN_MATCH_LEN];
        if !suffixes.contains_key(key) {
            suffixes.insert(key, pos);
        }
    }

    #[inline(always)]
    fn add_suffixes_interleaved_fast(
        data: &[u8],
        suffixes: &mut SuffixStore,
        start: usize,
        insert_limit: usize,
    ) {
        let lane = FAST_HASH_FILL_STEP;
        let mut pos = start;

        // Pipeline-ish fill: compute and retire several hash positions per loop
        // so the fastest path keeps multiple independent hash lookups in flight.
        while pos + lane * 3 < insert_limit {
            let p0 = pos;
            let p1 = pos + lane;
            let p2 = pos + lane * 2;
            let p3 = pos + lane * 3;

            Self::insert_suffix_if_absent(data, suffixes, p0);
            Self::insert_suffix_if_absent(data, suffixes, p1);
            Self::insert_suffix_if_absent(data, suffixes, p2);
            Self::insert_suffix_if_absent(data, suffixes, p3);

            pos += lane * 4;
        }

        while pos < insert_limit {
            Self::insert_suffix_if_absent(data, suffixes, pos);
            pos += lane;
        }
    }

    pub(crate) fn repcode_candidate(
        &self,
        data_slice: &[u8],
        literals_len: usize,
    ) -> Option<(usize, usize)> {
        if literals_len != 0 {
            return None;
        }

        let reps = [
            Some(self.offset_hist[1] as usize),
            Some(self.offset_hist[2] as usize),
            (self.offset_hist[0] > 1).then_some((self.offset_hist[0] - 1) as usize),
        ];

        let mut best: Option<(usize, usize)> = None;
        let mut seen = [0usize; 3];
        let mut seen_len = 0usize;
        for offset in reps.into_iter().flatten() {
            if offset == 0 {
                continue;
            }
            if seen[..seen_len].contains(&offset) {
                continue;
            }
            seen[seen_len] = offset;
            seen_len += 1;

            let Some(match_len) = self.offset_match_len(offset, data_slice) else {
                continue;
            };
            if match_len < MIN_MATCH_LEN {
                continue;
            }

            if best.is_none_or(|(old_offset, old_len)| {
                match_len > old_len || (match_len == old_len && offset < old_offset)
            }) {
                best = Some((offset, match_len));
            }
        }
        best
    }

    pub(crate) fn offset_match_len(&self, offset: usize, data_slice: &[u8]) -> Option<usize> {
        if offset == 0 {
            return None;
        }

        let last_idx = self.window.len().checked_sub(1)?;
        let last_entry = &self.window[last_idx];
        let searchable_prefix = self.window_size - (last_entry.data.len() - self.suffix_idx);
        if offset > searchable_prefix {
            return None;
        }

        let mut remaining = offset;
        let (entry_idx, match_index) = if remaining <= self.suffix_idx {
            (last_idx, self.suffix_idx - remaining)
        } else {
            remaining -= self.suffix_idx;
            let mut found = None;
            for entry_idx in (0..last_idx).rev() {
                let len = self.window[entry_idx].data.len();
                if remaining <= len {
                    found = Some((entry_idx, len - remaining));
                    break;
                }
                remaining -= len;
            }
            found?
        };

        let match_entry = &self.window[entry_idx];
        let match_slice = &match_entry.data[match_index..];

        Some(common_prefix_len(match_slice, data_slice))
    }

    /// Skip matching for the whole current window entry.
    ///
    /// When callers already know the block is incompressible, index positions
    /// sparsely and keep a dense tail so the next block still gets boundary
    /// matches.
    pub(crate) fn skip_matching_with_hint(&mut self, incompressible_hint: Option<bool>) {
        let len = self.window.last().unwrap().data.len();
        if incompressible_hint == Some(true) {
            let dense_tail = MIN_MATCH_LEN + INCOMPRESSIBLE_SKIP_STEP;
            let sparse_end = len.saturating_sub(dense_tail);
            self.add_suffixes_till(sparse_end, INCOMPRESSIBLE_SKIP_STEP);
            self.suffix_idx = sparse_end;
            self.add_suffixes_till(len, 1);
        } else {
            self.add_suffixes_till(len, 1);
        }
        self.suffix_idx = len;
        self.last_idx_in_sequence = len;
    }

    /// Backward-compatible dense path used by tests.
    #[cfg(test)]
    pub(crate) fn skip_matching(&mut self) {
        self.skip_matching_with_hint(None);
    }

    /// Add a new window entry. Will panic if the last window entry hasn't been processed properly.
    /// If any resources are released by pushing the new entry they are returned via the callback
    pub(crate) fn add_data(
        &mut self,
        data: Vec<u8>,
        suffixes: SuffixStore,
        reuse_space: impl FnMut(Vec<u8>, SuffixStore),
    ) {
        assert!(
            self.window.is_empty() || self.suffix_idx == self.window.last().unwrap().data.len()
        );
        self.reserve(data.len(), reuse_space);
        #[cfg(debug_assertions)]
        self.concat_window.extend_from_slice(&data);

        if let Some(last_len) = self.window.last().map(|last| last.data.len()) {
            for entry in self.window.iter_mut() {
                entry.base_offset += last_len;
            }
        }

        let len = data.len();
        self.window.push(WindowEntry {
            data,
            suffixes,
            base_offset: 0,
        });
        self.window_size += len;
        self.suffix_idx = 0;
        self.last_idx_in_sequence = 0;
    }

    /// Reserve space for a new window entry
    /// If any resources are released by pushing the new entry they are returned via the callback
    pub(crate) fn reserve(
        &mut self,
        amount: usize,
        mut reuse_space: impl FnMut(Vec<u8>, SuffixStore),
    ) {
        assert!(self.max_window_size >= amount);
        while self.window_size + amount > self.max_window_size {
            let removed = self.window.remove(0);
            self.window_size -= removed.data.len();
            #[cfg(debug_assertions)]
            self.concat_window.drain(0..removed.data.len());

            let WindowEntry {
                suffixes,
                data: leaked_vec,
                base_offset: _,
            } = removed;
            reuse_space(leaked_vec, suffixes);
        }
    }
}
