//! Matching algorithm used find repeated parts in the original data
//!
//! The Zstd format relies on finden repeated sequences of data and compressing these sequences as instructions to the decoder.
//! A sequence basically tells the decoder "Go back X bytes and copy Y bytes to the end of your decode buffer".
//!
//! The task here is to efficiently find matches in the already encoded data for the current suffix of the not yet encoded data.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::convert::TryInto;
use core::num::NonZeroUsize;

use super::CompressionLevel;
use super::Matcher;
use super::Sequence;
use super::blocks::encode_offset_with_history;

const MIN_MATCH_LEN: usize = 5;
const FAST_HASH_FILL_STEP: usize = 3;
const DFAST_MIN_MATCH_LEN: usize = 6;
const DFAST_TARGET_LEN: usize = 48;
// Keep these aligned with the issue's zstd level-3/dfast target unless ratio
// measurements show we can shrink them without regressing acceptance tests.
const DFAST_HASH_BITS: usize = 20;
const DFAST_SEARCH_DEPTH: usize = 4;
const DFAST_DEFAULT_WINDOW_SIZE: usize = 1 << 22;
const DFAST_EMPTY_SLOT: usize = usize::MAX;

#[derive(Copy, Clone, PartialEq, Eq)]
enum MatcherBackend {
    Simple,
    Dfast,
}

/// This is the default implementation of the `Matcher` trait. It allocates and reuses the buffers when possible.
pub struct MatchGeneratorDriver {
    vec_pool: Vec<Vec<u8>>,
    suffix_pool: Vec<SuffixStore>,
    match_generator: MatchGenerator,
    dfast_match_generator: Option<DfastMatchGenerator>,
    active_backend: MatcherBackend,
    slice_size: usize,
    base_slice_size: usize,
    base_window_size: usize,
    // Frame header window size must stay at the configured live-window budget.
    // Dictionary retention expands internal matcher capacity only.
    reported_window_size: usize,
}

impl MatchGeneratorDriver {
    /// slice_size says how big the slices should be that are allocated to work with
    /// max_slices_in_window says how many slices should at most be used while looking for matches
    pub(crate) fn new(slice_size: usize, max_slices_in_window: usize) -> Self {
        let max_window_size = max_slices_in_window * slice_size;
        Self {
            vec_pool: Vec::new(),
            suffix_pool: Vec::new(),
            match_generator: MatchGenerator::new(max_window_size),
            dfast_match_generator: None,
            active_backend: MatcherBackend::Simple,
            slice_size,
            base_slice_size: slice_size,
            base_window_size: max_window_size,
            reported_window_size: max_window_size,
        }
    }

    fn level_config(&self, level: CompressionLevel) -> (MatcherBackend, usize, usize, usize) {
        match level {
            CompressionLevel::Uncompressed => (
                MatcherBackend::Simple,
                self.base_slice_size,
                self.base_window_size,
                1,
            ),
            CompressionLevel::Fastest => (
                MatcherBackend::Simple,
                self.base_slice_size,
                self.base_window_size,
                FAST_HASH_FILL_STEP,
            ),
            CompressionLevel::Default => (
                MatcherBackend::Dfast,
                self.base_slice_size,
                DFAST_DEFAULT_WINDOW_SIZE,
                1,
            ),
            CompressionLevel::Better => (
                MatcherBackend::Simple,
                self.base_slice_size,
                self.base_window_size,
                1,
            ),
            CompressionLevel::Best => (
                MatcherBackend::Simple,
                self.base_slice_size,
                self.base_window_size,
                1,
            ),
        }
    }

    fn dfast_matcher(&self) -> &DfastMatchGenerator {
        self.dfast_match_generator
            .as_ref()
            .expect("dfast backend must be initialized by reset() before use")
    }

    fn dfast_matcher_mut(&mut self) -> &mut DfastMatchGenerator {
        self.dfast_match_generator
            .as_mut()
            .expect("dfast backend must be initialized by reset() before use")
    }
}

impl Matcher for MatchGeneratorDriver {
    fn reset(&mut self, level: CompressionLevel) {
        let (backend, slice_size, max_window_size, hash_fill_step) = self.level_config(level);
        if self.active_backend != backend {
            match self.active_backend {
                MatcherBackend::Simple => {
                    let vec_pool = &mut self.vec_pool;
                    let suffix_pool = &mut self.suffix_pool;
                    self.match_generator.reset(|mut data, mut suffixes| {
                        data.resize(data.capacity(), 0);
                        vec_pool.push(data);
                        suffixes.slots.clear();
                        suffixes.slots.resize(suffixes.slots.capacity(), None);
                        suffix_pool.push(suffixes);
                    });
                }
                MatcherBackend::Dfast => {
                    if let Some(dfast) = self.dfast_match_generator.as_mut() {
                        let vec_pool = &mut self.vec_pool;
                        dfast.reset(|mut data| {
                            data.resize(data.capacity(), 0);
                            vec_pool.push(data);
                        });
                    }
                }
            }
        }

        self.active_backend = backend;
        self.slice_size = slice_size;
        self.reported_window_size = max_window_size;
        match self.active_backend {
            MatcherBackend::Simple => {
                let vec_pool = &mut self.vec_pool;
                let suffix_pool = &mut self.suffix_pool;
                self.match_generator.max_window_size = max_window_size;
                self.match_generator.hash_fill_step = hash_fill_step;
                self.match_generator.reset(|mut data, mut suffixes| {
                    data.resize(data.capacity(), 0);
                    vec_pool.push(data);
                    suffixes.slots.clear();
                    suffixes.slots.resize(suffixes.slots.capacity(), None);
                    suffix_pool.push(suffixes);
                });
            }
            MatcherBackend::Dfast => {
                let dfast = self
                    .dfast_match_generator
                    .get_or_insert_with(|| DfastMatchGenerator::new(max_window_size));
                dfast.max_window_size = max_window_size;
                let vec_pool = &mut self.vec_pool;
                dfast.reset(|mut data| {
                    data.resize(data.capacity(), 0);
                    vec_pool.push(data);
                });
            }
        }
    }

    fn prime_with_dictionary(&mut self, dict_content: &[u8], offset_hist: [u32; 3]) {
        match self.active_backend {
            MatcherBackend::Simple => self.match_generator.offset_hist = offset_hist,
            MatcherBackend::Dfast => self.dfast_matcher_mut().offset_hist = offset_hist,
        }

        if dict_content.is_empty() {
            return;
        }

        // Dictionary bytes should stay addressable until produced frame output
        // itself exceeds the live window size.
        let retained_dict_budget = dict_content.len();
        match self.active_backend {
            MatcherBackend::Simple => {
                self.match_generator.max_window_size = self
                    .match_generator
                    .max_window_size
                    .saturating_add(retained_dict_budget);
            }
            MatcherBackend::Dfast => {
                let matcher = self.dfast_matcher_mut();
                matcher.max_window_size =
                    matcher.max_window_size.saturating_add(retained_dict_budget);
            }
        }

        let mut start = 0usize;
        let mut committed_dict_budget = 0usize;
        while start < dict_content.len() {
            let end = (start + self.slice_size).min(dict_content.len());
            if end - start < MIN_MATCH_LEN {
                break;
            }
            let mut space = self.get_next_space();
            space.clear();
            space.extend_from_slice(&dict_content[start..end]);
            self.commit_space(space);
            self.skip_matching();
            committed_dict_budget += end - start;
            start = end;
        }

        let uncommitted_tail_budget = retained_dict_budget.saturating_sub(committed_dict_budget);
        if uncommitted_tail_budget > 0 {
            match self.active_backend {
                MatcherBackend::Simple => {
                    self.match_generator.max_window_size = self
                        .match_generator
                        .max_window_size
                        .saturating_sub(uncommitted_tail_budget);
                }
                MatcherBackend::Dfast => {
                    let matcher = self.dfast_matcher_mut();
                    matcher.max_window_size = matcher
                        .max_window_size
                        .saturating_sub(uncommitted_tail_budget);
                }
            }
        }
    }

    fn window_size(&self) -> u64 {
        self.reported_window_size as u64
    }

    fn get_next_space(&mut self) -> Vec<u8> {
        self.vec_pool.pop().unwrap_or_else(|| {
            let mut space = alloc::vec![0; self.slice_size];
            space.resize(space.capacity(), 0);
            space
        })
    }

    fn get_last_space(&mut self) -> &[u8] {
        match self.active_backend {
            MatcherBackend::Simple => self.match_generator.window.last().unwrap().data.as_slice(),
            MatcherBackend::Dfast => self.dfast_matcher().get_last_space(),
        }
    }

    fn commit_space(&mut self, space: Vec<u8>) {
        match self.active_backend {
            MatcherBackend::Simple => {
                let vec_pool = &mut self.vec_pool;
                let suffixes = self
                    .suffix_pool
                    .pop()
                    .unwrap_or_else(|| SuffixStore::with_capacity(space.len()));
                let suffix_pool = &mut self.suffix_pool;
                self.match_generator
                    .add_data(space, suffixes, |mut data, mut suffixes| {
                        data.resize(data.capacity(), 0);
                        vec_pool.push(data);
                        suffixes.slots.clear();
                        suffixes.slots.resize(suffixes.slots.capacity(), None);
                        suffix_pool.push(suffixes);
                    });
            }
            MatcherBackend::Dfast => {
                let vec_pool = &mut self.vec_pool;
                self.dfast_match_generator
                    .as_mut()
                    .expect("dfast backend must be initialized by reset() before use")
                    .add_data(space, |mut data| {
                        data.resize(data.capacity(), 0);
                        vec_pool.push(data);
                    });
            }
        }
    }

    fn start_matching(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        match self.active_backend {
            MatcherBackend::Simple => {
                while self.match_generator.next_sequence(&mut handle_sequence) {}
            }
            MatcherBackend::Dfast => self
                .dfast_matcher_mut()
                .start_matching(&mut handle_sequence),
        }
    }
    fn skip_matching(&mut self) {
        match self.active_backend {
            MatcherBackend::Simple => self.match_generator.skip_matching(),
            MatcherBackend::Dfast => self.dfast_matcher_mut().skip_matching(),
        }
    }
}

/// This stores the index of a suffix of a string by hashing the first few bytes of that suffix
/// This means that collisions just overwrite and that you need to check validity after a get
struct SuffixStore {
    // We use NonZeroUsize to enable niche optimization here.
    // On store we do +1 and on get -1
    // This is ok since usize::MAX is never a valid offset
    slots: Vec<Option<NonZeroUsize>>,
    len_log: u32,
}

impl SuffixStore {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            slots: alloc::vec![None; capacity],
            len_log: capacity.ilog2(),
        }
    }

    #[inline(always)]
    fn insert(&mut self, suffix: &[u8], idx: usize) {
        let key = self.key(suffix);
        self.slots[key] = Some(NonZeroUsize::new(idx + 1).unwrap());
    }

    #[inline(always)]
    fn contains_key(&self, suffix: &[u8]) -> bool {
        let key = self.key(suffix);
        self.slots[key].is_some()
    }

    #[inline(always)]
    fn get(&self, suffix: &[u8]) -> Option<usize> {
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
struct WindowEntry {
    data: Vec<u8>,
    /// Stores indexes into data
    suffixes: SuffixStore,
    /// Makes offset calculations efficient
    base_offset: usize,
}

pub(crate) struct MatchGenerator {
    max_window_size: usize,
    /// Data window we are operating on to find matches
    /// The data we want to find matches for is in the last slice
    window: Vec<WindowEntry>,
    window_size: usize,
    #[cfg(debug_assertions)]
    concat_window: Vec<u8>,
    /// Index in the last slice that we already processed
    suffix_idx: usize,
    /// Gets updated when a new sequence is returned to point right behind that sequence
    last_idx_in_sequence: usize,
    hash_fill_step: usize,
    offset_hist: [u32; 3],
}

impl MatchGenerator {
    #[inline(always)]
    #[cfg(target_endian = "little")]
    fn mismatch_byte_index(diff: usize) -> usize {
        diff.trailing_zeros() as usize / 8
    }

    #[inline(always)]
    #[cfg(target_endian = "big")]
    fn mismatch_byte_index(diff: usize) -> usize {
        diff.leading_zeros() as usize / 8
    }

    /// max_size defines how many bytes will be used at most in the window used for matching
    fn new(max_size: usize) -> Self {
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

    fn reset(&mut self, mut reuse_space: impl FnMut(Vec<u8>, SuffixStore)) {
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
    fn next_sequence(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) -> bool {
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
                    let match_len = Self::common_prefix_len(match_slice, data_slice);

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

    /// Find the common prefix length between two byte slices
    #[inline(always)]
    fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
        let max = a.len().min(b.len());
        let chunk = core::mem::size_of::<usize>();
        let mut off = 0usize;
        let lhs = a.as_ptr();
        let rhs = b.as_ptr();

        while off + chunk <= max {
            let lhs_word = unsafe { core::ptr::read_unaligned(lhs.add(off) as *const usize) };
            let rhs_word = unsafe { core::ptr::read_unaligned(rhs.add(off) as *const usize) };
            let diff = lhs_word ^ rhs_word;
            if diff != 0 {
                return off + Self::mismatch_byte_index(diff);
            }
            off += chunk;
        }

        off + core::iter::zip(&a[off..max], &b[off..max])
            .take_while(|(x, y)| x == y)
            .count()
    }

    /// Process bytes and add the suffixes to the suffix store up to a specific index
    #[inline(always)]
    fn add_suffixes_till(&mut self, idx: usize, fill_step: usize) {
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

    fn repcode_candidate(&self, data_slice: &[u8], literals_len: usize) -> Option<(usize, usize)> {
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

    fn offset_match_len(&self, offset: usize, data_slice: &[u8]) -> Option<usize> {
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

        Some(Self::common_prefix_len(match_slice, data_slice))
    }

    /// Skip matching for the whole current window entry
    fn skip_matching(&mut self) {
        let len = self.window.last().unwrap().data.len();
        self.add_suffixes_till(len, 1);
        self.suffix_idx = len;
        self.last_idx_in_sequence = len;
    }

    /// Add a new window entry. Will panic if the last window entry hasn't been processed properly.
    /// If any resources are released by pushing the new entry they are returned via the callback
    fn add_data(
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
    fn reserve(&mut self, amount: usize, mut reuse_space: impl FnMut(Vec<u8>, SuffixStore)) {
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

struct DfastMatchGenerator {
    max_window_size: usize,
    window: VecDeque<Vec<u8>>,
    window_size: usize,
    // We keep a contiguous searchable history to avoid rebuilding and reseeding
    // the matcher state from disjoint block buffers on every block.
    history: Vec<u8>,
    history_start: usize,
    history_abs_start: usize,
    offset_hist: [u32; 3],
    short_hash: Vec<[usize; DFAST_SEARCH_DEPTH]>,
    long_hash: Vec<[usize; DFAST_SEARCH_DEPTH]>,
}

#[derive(Copy, Clone)]
struct MatchCandidate {
    start: usize,
    offset: usize,
    match_len: usize,
}

impl DfastMatchGenerator {
    fn new(max_window_size: usize) -> Self {
        Self {
            max_window_size,
            window: VecDeque::new(),
            window_size: 0,
            history: Vec::new(),
            history_start: 0,
            history_abs_start: 0,
            offset_hist: [1, 4, 8],
            short_hash: Vec::new(),
            long_hash: Vec::new(),
        }
    }

    fn reset(&mut self, mut reuse_space: impl FnMut(Vec<u8>)) {
        self.window_size = 0;
        self.history.clear();
        self.history_start = 0;
        self.history_abs_start = 0;
        self.offset_hist = [1, 4, 8];
        if !self.short_hash.is_empty() {
            self.short_hash.fill([DFAST_EMPTY_SLOT; DFAST_SEARCH_DEPTH]);
            self.long_hash.fill([DFAST_EMPTY_SLOT; DFAST_SEARCH_DEPTH]);
        }
        for mut data in self.window.drain(..) {
            data.resize(data.capacity(), 0);
            reuse_space(data);
        }
    }

    fn get_last_space(&self) -> &[u8] {
        self.window.back().unwrap().as_slice()
    }

    fn add_data(&mut self, data: Vec<u8>, mut reuse_space: impl FnMut(Vec<u8>)) {
        assert!(data.len() <= self.max_window_size);
        while self.window_size + data.len() > self.max_window_size {
            let mut removed = self.window.pop_front().unwrap();
            self.window_size -= removed.len();
            self.history_start += removed.len();
            self.history_abs_start += removed.len();
            removed.resize(removed.capacity(), 0);
            reuse_space(removed);
        }
        self.compact_history();
        self.history.extend_from_slice(&data);
        self.window_size += data.len();
        self.window.push_back(data);
    }

    fn skip_matching(&mut self) {
        self.ensure_hash_tables();
        let current_len = self.window.back().unwrap().len();
        let current_abs_start = self.history_abs_start + self.window_size - current_len;
        self.insert_positions(current_abs_start, current_abs_start + current_len);
    }

    fn start_matching(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        self.ensure_hash_tables();

        let current_len = self.window.back().unwrap().len();
        if current_len == 0 {
            return;
        }

        let current_abs_start = self.history_abs_start + self.window_size - current_len;

        let mut pos = 0usize;
        let mut literals_start = 0usize;
        while pos + DFAST_MIN_MATCH_LEN <= current_len {
            let abs_pos = current_abs_start + pos;
            let lit_len = pos - literals_start;

            let best = self.best_match(abs_pos, lit_len);
            if let Some(candidate) = self.pick_lazy_match(abs_pos, lit_len, best) {
                self.insert_positions(abs_pos, candidate.start + candidate.match_len);
                let current = self.window.back().unwrap().as_slice();
                let start = candidate.start - current_abs_start;
                let literals = &current[literals_start..start];
                handle_sequence(Sequence::Triple {
                    literals,
                    offset: candidate.offset,
                    match_len: candidate.match_len,
                });
                // The encoded offset value is irrelevant here; we only need the
                // side effect on offset history for future rep-code matching.
                let _ = encode_offset_with_history(
                    candidate.offset as u32,
                    literals.len() as u32,
                    &mut self.offset_hist,
                );
                pos = start + candidate.match_len;
                literals_start = pos;
            } else {
                self.insert_position(abs_pos);
                pos += 1;
            }
        }

        if literals_start < current_len {
            // We stop inserting once fewer than DFAST_MIN_MATCH_LEN bytes remain.
            // The last boundary-spanning start that can seed a dfast hash is
            // still inserted by the loop above; `dfast_inserts_tail_positions_
            // for_next_block_matching()` asserts that the next block can match
            // immediately at the boundary without eagerly seeding the final
            // DFAST_MIN_MATCH_LEN - 1 bytes here.
            let current = self.window.back().unwrap().as_slice();
            handle_sequence(Sequence::Literals {
                literals: &current[literals_start..],
            });
        }
    }

    fn ensure_hash_tables(&mut self) {
        if self.short_hash.is_empty() {
            // This is intentionally lazy so Fastest/Uncompressed never pay the
            // ~dfast-level memory cost. The current size tracks the issue's
            // zstd level-3 style parameters rather than a generic low-memory preset.
            self.short_hash =
                alloc::vec![[DFAST_EMPTY_SLOT; DFAST_SEARCH_DEPTH]; 1 << DFAST_HASH_BITS];
            self.long_hash =
                alloc::vec![[DFAST_EMPTY_SLOT; DFAST_SEARCH_DEPTH]; 1 << DFAST_HASH_BITS];
        }
    }

    fn compact_history(&mut self) {
        if self.history_start == 0 {
            return;
        }
        if self.history_start >= self.max_window_size
            || self.history_start * 2 >= self.history.len()
        {
            self.history.drain(..self.history_start);
            self.history_start = 0;
        }
    }

    fn live_history(&self) -> &[u8] {
        &self.history[self.history_start..]
    }

    fn history_abs_end(&self) -> usize {
        self.history_abs_start + self.live_history().len()
    }

    fn best_match(&self, abs_pos: usize, lit_len: usize) -> Option<MatchCandidate> {
        let rep = self.repcode_candidate(abs_pos, lit_len);
        let hash = self.hash_candidate(abs_pos, lit_len);
        Self::better_candidate(rep, hash)
    }

    fn pick_lazy_match(
        &self,
        abs_pos: usize,
        lit_len: usize,
        best: Option<MatchCandidate>,
    ) -> Option<MatchCandidate> {
        let best = best?;
        if best.match_len >= DFAST_TARGET_LEN
            || abs_pos + 1 + DFAST_MIN_MATCH_LEN > self.history_abs_end()
        {
            return Some(best);
        }

        let next = self.best_match(abs_pos + 1, lit_len + 1);
        match next {
            Some(next)
                if next.match_len > best.match_len
                    || (next.match_len == best.match_len && next.offset < best.offset) =>
            {
                None
            }
            _ => Some(best),
        }
    }

    fn repcode_candidate(&self, abs_pos: usize, lit_len: usize) -> Option<MatchCandidate> {
        let reps = if lit_len == 0 {
            [
                Some(self.offset_hist[1] as usize),
                Some(self.offset_hist[2] as usize),
                (self.offset_hist[0] > 1).then_some((self.offset_hist[0] - 1) as usize),
            ]
        } else {
            [
                Some(self.offset_hist[0] as usize),
                Some(self.offset_hist[1] as usize),
                Some(self.offset_hist[2] as usize),
            ]
        };

        let mut best = None;
        for rep in reps.into_iter().flatten() {
            if rep == 0 || rep > abs_pos {
                continue;
            }
            let candidate_pos = abs_pos - rep;
            if candidate_pos < self.history_abs_start {
                continue;
            }
            let concat = self.live_history();
            let candidate_idx = candidate_pos - self.history_abs_start;
            let current_idx = abs_pos - self.history_abs_start;
            if current_idx + DFAST_MIN_MATCH_LEN > concat.len() {
                continue;
            }
            let match_len =
                MatchGenerator::common_prefix_len(&concat[candidate_idx..], &concat[current_idx..]);
            if match_len >= DFAST_MIN_MATCH_LEN {
                let candidate = self.extend_backwards(candidate_pos, abs_pos, match_len, lit_len);
                best = Self::better_candidate(best, Some(candidate));
            }
        }
        best
    }

    fn hash_candidate(&self, abs_pos: usize, lit_len: usize) -> Option<MatchCandidate> {
        let concat = self.live_history();
        let current_idx = abs_pos - self.history_abs_start;
        let mut best = None;
        for candidate_pos in self.long_candidates(abs_pos) {
            if candidate_pos < self.history_abs_start || candidate_pos >= abs_pos {
                continue;
            }
            let candidate_idx = candidate_pos - self.history_abs_start;
            let match_len =
                MatchGenerator::common_prefix_len(&concat[candidate_idx..], &concat[current_idx..]);
            if match_len >= DFAST_MIN_MATCH_LEN {
                let candidate = self.extend_backwards(candidate_pos, abs_pos, match_len, lit_len);
                best = Self::better_candidate(best, Some(candidate));
                if best.is_some_and(|best| best.match_len >= DFAST_TARGET_LEN) {
                    return best;
                }
            }
        }

        for candidate_pos in self.short_candidates(abs_pos) {
            if candidate_pos < self.history_abs_start || candidate_pos >= abs_pos {
                continue;
            }
            let candidate_idx = candidate_pos - self.history_abs_start;
            let match_len =
                MatchGenerator::common_prefix_len(&concat[candidate_idx..], &concat[current_idx..]);
            if match_len >= DFAST_MIN_MATCH_LEN {
                let candidate = self.extend_backwards(candidate_pos, abs_pos, match_len, lit_len);
                best = Self::better_candidate(best, Some(candidate));
                if best.is_some_and(|best| best.match_len >= DFAST_TARGET_LEN) {
                    return best;
                }
            }
        }
        best
    }

    fn extend_backwards(
        &self,
        mut candidate_pos: usize,
        mut abs_pos: usize,
        mut match_len: usize,
        lit_len: usize,
    ) -> MatchCandidate {
        let concat = self.live_history();
        let min_abs_pos = abs_pos - lit_len;
        while abs_pos > min_abs_pos
            && candidate_pos > self.history_abs_start
            && concat[candidate_pos - self.history_abs_start - 1]
                == concat[abs_pos - self.history_abs_start - 1]
        {
            candidate_pos -= 1;
            abs_pos -= 1;
            match_len += 1;
        }
        MatchCandidate {
            start: abs_pos,
            offset: abs_pos - candidate_pos,
            match_len,
        }
    }

    fn better_candidate(
        lhs: Option<MatchCandidate>,
        rhs: Option<MatchCandidate>,
    ) -> Option<MatchCandidate> {
        match (lhs, rhs) {
            (None, other) | (other, None) => other,
            (Some(lhs), Some(rhs)) => {
                if rhs.match_len > lhs.match_len
                    || (rhs.match_len == lhs.match_len && rhs.offset < lhs.offset)
                {
                    Some(rhs)
                } else {
                    Some(lhs)
                }
            }
        }
    }

    fn insert_positions(&mut self, start: usize, end: usize) {
        for pos in start..end {
            self.insert_position(pos);
        }
    }

    fn insert_position(&mut self, pos: usize) {
        let idx = pos - self.history_abs_start;
        let short = {
            let concat = self.live_history();
            (idx + 4 <= concat.len()).then(|| Self::hash4(&concat[idx..]))
        };
        if let Some(short) = short {
            let bucket = &mut self.short_hash[short];
            if bucket[0] != pos {
                bucket.copy_within(0..DFAST_SEARCH_DEPTH - 1, 1);
                bucket[0] = pos;
            }
        }

        let long = {
            let concat = self.live_history();
            (idx + 8 <= concat.len()).then(|| Self::hash8(&concat[idx..]))
        };
        if let Some(long) = long {
            let bucket = &mut self.long_hash[long];
            if bucket[0] != pos {
                bucket.copy_within(0..DFAST_SEARCH_DEPTH - 1, 1);
                bucket[0] = pos;
            }
        }
    }

    fn short_candidates(&self, pos: usize) -> impl Iterator<Item = usize> + '_ {
        let concat = self.live_history();
        let idx = pos - self.history_abs_start;
        (idx + 4 <= concat.len())
            .then(|| self.short_hash[Self::hash4(&concat[idx..])])
            .into_iter()
            .flatten()
            .filter(|candidate| *candidate != DFAST_EMPTY_SLOT)
    }

    fn long_candidates(&self, pos: usize) -> impl Iterator<Item = usize> + '_ {
        let concat = self.live_history();
        let idx = pos - self.history_abs_start;
        (idx + 8 <= concat.len())
            .then(|| self.long_hash[Self::hash8(&concat[idx..])])
            .into_iter()
            .flatten()
            .filter(|candidate| *candidate != DFAST_EMPTY_SLOT)
    }

    fn hash4(data: &[u8]) -> usize {
        let value = u32::from_le_bytes(data[..4].try_into().unwrap()) as u64;
        Self::hash_bits(value)
    }

    fn hash8(data: &[u8]) -> usize {
        let value = u64::from_le_bytes(data[..8].try_into().unwrap());
        Self::hash_bits(value)
    }

    fn hash_bits(value: u64) -> usize {
        const PRIME: u64 = 0x9E37_79B1_85EB_CA87;
        ((value.wrapping_mul(PRIME)) >> (64 - DFAST_HASH_BITS)) as usize
    }
}

#[test]
fn matches() {
    let mut matcher = MatchGenerator::new(1000);
    let mut original_data = Vec::new();
    let mut reconstructed = Vec::new();

    let replay_sequence = |seq: Sequence<'_>, reconstructed: &mut Vec<u8>| match seq {
        Sequence::Literals { literals } => {
            assert!(!literals.is_empty());
            reconstructed.extend_from_slice(literals);
        }
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            assert!(offset > 0);
            assert!(match_len >= MIN_MATCH_LEN);
            reconstructed.extend_from_slice(literals);
            assert!(offset <= reconstructed.len());
            let start = reconstructed.len() - offset;
            for i in 0..match_len {
                let byte = reconstructed[start + i];
                reconstructed.push(byte);
            }
        }
    };

    matcher.add_data(
        alloc::vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));

    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![
            1, 2, 3, 4, 5, 6, 1, 2, 3, 4, 5, 6, 1, 2, 3, 4, 5, 6, 0, 0, 0, 0, 0,
        ],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[
        1, 2, 3, 4, 5, 6, 1, 2, 3, 4, 5, 6, 1, 2, 3, 4, 5, 6, 0, 0, 0, 0, 0,
    ]);

    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 0, 0, 0, 0, 0],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 0, 0, 0, 0, 0]);

    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![0, 0, 0, 0, 0],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[0, 0, 0, 0, 0]);

    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![7, 8, 9, 10, 11],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[7, 8, 9, 10, 11]);

    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![1, 3, 5, 7, 9],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    matcher.skip_matching();
    original_data.extend_from_slice(&[1, 3, 5, 7, 9]);
    reconstructed.extend_from_slice(&[1, 3, 5, 7, 9]);
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![1, 3, 5, 7, 9],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[1, 3, 5, 7, 9]);

    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![0, 0, 11, 13, 15, 17, 20, 11, 13, 15, 17, 20, 21, 23],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[0, 0, 11, 13, 15, 17, 20, 11, 13, 15, 17, 20, 21, 23]);

    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    assert!(!matcher.next_sequence(|_| {}));

    assert_eq!(reconstructed, original_data);
}

#[test]
fn dfast_matches_roundtrip_multi_block_pattern() {
    let pattern = [9, 21, 44, 184, 19, 96, 171, 109, 141, 251];
    let first_block: Vec<u8> = pattern.iter().copied().cycle().take(128 * 1024).collect();
    let second_block: Vec<u8> = pattern.iter().copied().cycle().take(128 * 1024).collect();

    let mut matcher = DfastMatchGenerator::new(DFAST_DEFAULT_WINDOW_SIZE);
    let replay_sequence = |decoded: &mut Vec<u8>, seq: Sequence<'_>| match seq {
        Sequence::Literals { literals } => decoded.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            decoded.extend_from_slice(literals);
            let start = decoded.len() - offset;
            for i in 0..match_len {
                let byte = decoded[start + i];
                decoded.push(byte);
            }
        }
    };

    matcher.add_data(first_block.clone(), |_| {});
    let mut history = Vec::new();
    matcher.start_matching(|seq| replay_sequence(&mut history, seq));
    assert_eq!(history, first_block);

    matcher.add_data(second_block.clone(), |_| {});
    let prefix_len = history.len();
    matcher.start_matching(|seq| replay_sequence(&mut history, seq));

    assert_eq!(&history[prefix_len..], second_block.as_slice());
}

#[test]
fn driver_switches_backends_and_initializes_dfast_via_reset() {
    let mut driver = MatchGeneratorDriver::new(32, 2);

    driver.reset(CompressionLevel::Default);
    assert_eq!(driver.window_size(), DFAST_DEFAULT_WINDOW_SIZE as u64);

    let mut first = driver.get_next_space();
    first[..12].copy_from_slice(b"abcabcabcabc");
    first.truncate(12);
    driver.commit_space(first);
    assert_eq!(driver.get_last_space(), b"abcabcabcabc");
    driver.skip_matching();

    let mut second = driver.get_next_space();
    second[..12].copy_from_slice(b"abcabcabcabc");
    second.truncate(12);
    driver.commit_space(second);

    let mut reconstructed = b"abcabcabcabc".to_vec();
    driver.start_matching(|seq| match seq {
        Sequence::Literals { literals } => reconstructed.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            reconstructed.extend_from_slice(literals);
            let start = reconstructed.len() - offset;
            for i in 0..match_len {
                let byte = reconstructed[start + i];
                reconstructed.push(byte);
            }
        }
    });
    assert_eq!(reconstructed, b"abcabcabcabcabcabcabcabc");

    driver.reset(CompressionLevel::Fastest);
    assert_eq!(driver.window_size(), 64);
}

#[test]
fn prime_with_dictionary_preserves_history_for_first_full_block() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Fastest);

    driver.prime_with_dictionary(b"abcdefgh", [1, 4, 8]);

    let mut space = driver.get_next_space();
    space.clear();
    space.extend_from_slice(b"abcdefgh");
    driver.commit_space(space);

    let mut saw_match = false;
    driver.start_matching(|seq| {
        if let Sequence::Triple {
            literals,
            offset,
            match_len,
        } = seq
            && literals.is_empty()
            && offset == 8
            && match_len >= MIN_MATCH_LEN
        {
            saw_match = true;
        }
    });

    assert!(
        saw_match,
        "first full block should still match dictionary-primed history"
    );
}

#[test]
fn prime_with_large_dictionary_preserves_early_history_until_first_block() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Fastest);

    driver.prime_with_dictionary(b"abcdefghABCDEFGHijklmnop", [1, 4, 8]);

    let mut space = driver.get_next_space();
    space.clear();
    space.extend_from_slice(b"abcdefgh");
    driver.commit_space(space);

    let mut saw_match = false;
    driver.start_matching(|seq| {
        if let Sequence::Triple {
            literals,
            offset,
            match_len,
        } = seq
            && literals.is_empty()
            && offset == 24
            && match_len >= MIN_MATCH_LEN
        {
            saw_match = true;
        }
    });

    assert!(
        saw_match,
        "dictionary bytes should remain addressable until frame output exceeds the live window"
    );
}

#[test]
fn prime_with_dictionary_applies_offset_history_even_when_content_is_empty() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Fastest);

    driver.prime_with_dictionary(&[], [11, 7, 3]);

    assert_eq!(driver.match_generator.offset_hist, [11, 7, 3]);
}

#[test]
fn dfast_prime_with_dictionary_preserves_history_for_first_full_block() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Default);

    driver.prime_with_dictionary(b"abcdefgh", [1, 4, 8]);

    let mut space = driver.get_next_space();
    space.clear();
    space.extend_from_slice(b"abcdefgh");
    driver.commit_space(space);

    let mut saw_match = false;
    driver.start_matching(|seq| {
        if let Sequence::Triple {
            literals,
            offset,
            match_len,
        } = seq
            && literals.is_empty()
            && offset == 8
            && match_len >= DFAST_MIN_MATCH_LEN
        {
            saw_match = true;
        }
    });

    assert!(
        saw_match,
        "dfast backend should match dictionary-primed history in first full block"
    );
}

#[test]
fn prime_with_dictionary_does_not_inflate_reported_window_size() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Fastest);

    let before = driver.window_size();
    driver.prime_with_dictionary(b"abcdefghABCDEFGHijklmnop", [1, 4, 8]);
    let after = driver.window_size();

    assert_eq!(
        after, before,
        "dictionary retention budget must not change reported frame window size"
    );
}

#[test]
fn prime_with_dictionary_does_not_reuse_tiny_suffix_store() {
    let mut driver = MatchGeneratorDriver::new(8, 2);
    driver.reset(CompressionLevel::Fastest);

    // This dictionary leaves a 1-byte tail chunk (capacity=1 suffix table),
    // which should never be committed to the matcher window.
    driver.prime_with_dictionary(b"abcdefghi", [1, 4, 8]);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for block in [b"mnopqrstu", b"vwxyzabcd", b"efghijklm"] {
            let mut space = driver.get_next_space();
            space.clear();
            space.extend_from_slice(block);
            driver.commit_space(space);
            driver.skip_matching();
        }
    }));

    assert!(
        result.is_ok(),
        "tiny dictionary tail must not poison suffix store reuse"
    );
}

#[test]
fn prime_with_dictionary_counts_only_committed_tail_budget() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Fastest);

    let before = driver.match_generator.max_window_size;
    // One full slice plus a 1-byte tail that cannot be committed.
    driver.prime_with_dictionary(b"abcdefghi", [1, 4, 8]);

    assert_eq!(
        driver.match_generator.max_window_size,
        before + 8,
        "retention budget must account only for dictionary bytes actually committed to history"
    );
}

#[test]
fn suffix_store_with_single_slot_does_not_panic_on_keying() {
    let mut suffixes = SuffixStore::with_capacity(1);
    suffixes.insert(b"abcde", 0);
    assert!(suffixes.contains_key(b"abcde"));
    assert_eq!(suffixes.get(b"abcde"), Some(0));
}

#[test]
fn fastest_reset_uses_interleaved_hash_fill_step() {
    let mut driver = MatchGeneratorDriver::new(32, 2);

    driver.reset(CompressionLevel::Uncompressed);
    assert_eq!(driver.match_generator.hash_fill_step, 1);

    driver.reset(CompressionLevel::Fastest);
    assert_eq!(driver.match_generator.hash_fill_step, FAST_HASH_FILL_STEP);

    driver.reset(CompressionLevel::Better);
    assert_eq!(driver.match_generator.hash_fill_step, 1);
}

#[test]
fn simple_matcher_updates_offset_history_after_emitting_match() {
    let mut matcher = MatchGenerator::new(64);
    matcher.add_data(
        b"abcdeabcdeabcde".to_vec(),
        SuffixStore::with_capacity(64),
        |_, _| {},
    );

    assert!(matcher.next_sequence(|seq| {
        assert_eq!(
            seq,
            Sequence::Triple {
                literals: b"abcde",
                offset: 5,
                match_len: 10,
            }
        );
    }));
    assert_eq!(matcher.offset_hist, [5, 1, 4]);
}

#[test]
fn simple_matcher_zero_literal_repcode_checks_rep1_before_hash_lookup() {
    let mut matcher = MatchGenerator::new(64);
    matcher.add_data(
        b"abcdefghijabcdefghij".to_vec(),
        SuffixStore::with_capacity(64),
        |_, _| {},
    );

    matcher.suffix_idx = 10;
    matcher.last_idx_in_sequence = 10;
    matcher.offset_hist = [99, 10, 4];

    let candidate = matcher.repcode_candidate(&matcher.window.last().unwrap().data[10..], 0);
    assert_eq!(candidate, Some((10, 10)));
}

#[test]
fn simple_matcher_repcode_can_target_previous_window_entry() {
    let mut matcher = MatchGenerator::new(64);
    matcher.add_data(
        b"abcdefghij".to_vec(),
        SuffixStore::with_capacity(64),
        |_, _| {},
    );
    matcher.skip_matching();
    matcher.add_data(
        b"abcdefghij".to_vec(),
        SuffixStore::with_capacity(64),
        |_, _| {},
    );

    matcher.offset_hist = [99, 10, 4];

    let candidate = matcher.repcode_candidate(&matcher.window.last().unwrap().data, 0);
    assert_eq!(candidate, Some((10, 10)));
}

#[test]
fn simple_matcher_zero_literal_repcode_checks_rep2() {
    let mut matcher = MatchGenerator::new(64);
    matcher.add_data(
        b"abcdefghijabcdefghij".to_vec(),
        SuffixStore::with_capacity(64),
        |_, _| {},
    );
    matcher.suffix_idx = 10;
    matcher.last_idx_in_sequence = 10;
    // rep1=4 does not match at idx 10, rep2=10 does.
    matcher.offset_hist = [99, 4, 10];

    let candidate = matcher.repcode_candidate(&matcher.window.last().unwrap().data[10..], 0);
    assert_eq!(candidate, Some((10, 10)));
}

#[test]
fn simple_matcher_zero_literal_repcode_checks_rep0_minus1() {
    let mut matcher = MatchGenerator::new(64);
    matcher.add_data(
        b"abcdefghijabcdefghij".to_vec(),
        SuffixStore::with_capacity(64),
        |_, _| {},
    );
    matcher.suffix_idx = 10;
    matcher.last_idx_in_sequence = 10;
    // rep1=4 and rep2=99 do not match; rep0-1 == 10 does.
    matcher.offset_hist = [11, 4, 99];

    let candidate = matcher.repcode_candidate(&matcher.window.last().unwrap().data[10..], 0);
    assert_eq!(candidate, Some((10, 10)));
}

#[test]
fn simple_matcher_repcode_rejects_offsets_beyond_searchable_prefix() {
    let mut matcher = MatchGenerator::new(64);
    matcher.add_data(
        b"abcdefghij".to_vec(),
        SuffixStore::with_capacity(64),
        |_, _| {},
    );
    matcher.skip_matching();
    matcher.add_data(
        b"klmnopqrst".to_vec(),
        SuffixStore::with_capacity(64),
        |_, _| {},
    );
    matcher.suffix_idx = 3;

    let candidate = matcher.offset_match_len(14, &matcher.window.last().unwrap().data[3..]);
    assert_eq!(candidate, None);
}

#[test]
fn simple_matcher_skip_matching_seeds_every_position_even_with_fast_step() {
    let mut matcher = MatchGenerator::new(64);
    matcher.hash_fill_step = FAST_HASH_FILL_STEP;
    matcher.add_data(
        b"abcdefghijklmnop".to_vec(),
        SuffixStore::with_capacity(64),
        |_, _| {},
    );
    matcher.skip_matching();
    matcher.add_data(b"bcdef".to_vec(), SuffixStore::with_capacity(64), |_, _| {});

    assert!(matcher.next_sequence(|seq| {
        assert_eq!(
            seq,
            Sequence::Triple {
                literals: b"",
                offset: 15,
                match_len: 5,
            }
        );
    }));
    assert!(!matcher.next_sequence(|_| {}));
}

#[test]
fn simple_matcher_add_suffixes_till_backfills_last_searchable_anchor() {
    let mut matcher = MatchGenerator::new(64);
    matcher.hash_fill_step = FAST_HASH_FILL_STEP;
    matcher.add_data(
        b"01234abcde".to_vec(),
        SuffixStore::with_capacity(64),
        |_, _| {},
    );
    matcher.add_suffixes_till(10, FAST_HASH_FILL_STEP);

    let last = matcher.window.last().unwrap();
    let tail = &last.data[5..10];
    assert_eq!(last.suffixes.get(tail), Some(5));
}

#[test]
fn simple_matcher_add_suffixes_till_skips_when_idx_below_min_match_len() {
    let mut matcher = MatchGenerator::new(128);
    matcher.hash_fill_step = FAST_HASH_FILL_STEP;
    matcher.add_data(
        b"abcdefghijklmnopqrstuvwxyz".to_vec(),
        SuffixStore::with_capacity(1 << 16),
        |_, _| {},
    );

    matcher.add_suffixes_till(MIN_MATCH_LEN - 1, FAST_HASH_FILL_STEP);

    let last = matcher.window.last().unwrap();
    let first_key = &last.data[..MIN_MATCH_LEN];
    assert_eq!(last.suffixes.get(first_key), None);
}

#[test]
fn simple_matcher_add_suffixes_till_fast_step_registers_interleaved_positions() {
    let mut matcher = MatchGenerator::new(128);
    matcher.hash_fill_step = FAST_HASH_FILL_STEP;
    matcher.add_data(
        b"abcdefghijklmnopqrstuvwxyz".to_vec(),
        SuffixStore::with_capacity(1 << 16),
        |_, _| {},
    );

    matcher.add_suffixes_till(17, FAST_HASH_FILL_STEP);

    let last = matcher.window.last().unwrap();
    for pos in [0usize, 3, 6, 9, 12] {
        let key = &last.data[pos..pos + MIN_MATCH_LEN];
        assert_eq!(
            last.suffixes.get(key),
            Some(pos),
            "expected interleaved suffix registration at pos {pos}"
        );
    }
}

#[test]
fn dfast_skip_matching_handles_window_eviction() {
    let mut matcher = DfastMatchGenerator::new(16);

    matcher.add_data(alloc::vec![1, 2, 3, 4, 5, 6], |_| {});
    matcher.skip_matching();
    matcher.add_data(alloc::vec![7, 8, 9, 10, 11, 12], |_| {});
    matcher.skip_matching();
    matcher.add_data(alloc::vec![7, 8, 9, 10, 11, 12], |_| {});

    let mut reconstructed = alloc::vec![7, 8, 9, 10, 11, 12];
    matcher.start_matching(|seq| match seq {
        Sequence::Literals { literals } => reconstructed.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            reconstructed.extend_from_slice(literals);
            let start = reconstructed.len() - offset;
            for i in 0..match_len {
                let byte = reconstructed[start + i];
                reconstructed.push(byte);
            }
        }
    });

    assert_eq!(reconstructed, [7, 8, 9, 10, 11, 12, 7, 8, 9, 10, 11, 12]);
}

#[test]
fn dfast_inserts_tail_positions_for_next_block_matching() {
    let mut matcher = DfastMatchGenerator::new(DFAST_DEFAULT_WINDOW_SIZE);

    matcher.add_data(b"012345bcdea".to_vec(), |_| {});
    let mut history = Vec::new();
    matcher.start_matching(|seq| match seq {
        Sequence::Literals { literals } => history.extend_from_slice(literals),
        Sequence::Triple { .. } => unreachable!("first block should not match history"),
    });
    assert_eq!(history, b"012345bcdea");

    matcher.add_data(b"bcdeabcdeab".to_vec(), |_| {});
    let mut saw_first_sequence = false;
    matcher.start_matching(|seq| {
        assert!(!saw_first_sequence, "expected a single cross-block match");
        saw_first_sequence = true;
        match seq {
            Sequence::Literals { .. } => {
                panic!("expected tail-anchored cross-block match before any literals")
            }
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => {
                assert_eq!(literals, b"");
                assert_eq!(offset, 5);
                assert_eq!(match_len, 11);
                let start = history.len() - offset;
                for i in 0..match_len {
                    let byte = history[start + i];
                    history.push(byte);
                }
            }
        }
    });

    assert!(
        saw_first_sequence,
        "expected tail-anchored cross-block match"
    );
    assert_eq!(history, b"012345bcdeabcdeabcdeab");
}
