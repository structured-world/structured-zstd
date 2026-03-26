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
const DFAST_MIN_MATCH_LEN: usize = 6;
const DFAST_TARGET_LEN: usize = 48;
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
        }
    }

    fn level_config(&self, level: CompressionLevel) -> (MatcherBackend, usize, usize) {
        match level {
            CompressionLevel::Uncompressed => (
                MatcherBackend::Simple,
                self.base_slice_size,
                self.base_window_size,
            ),
            CompressionLevel::Fastest => (
                MatcherBackend::Simple,
                self.base_slice_size,
                self.base_window_size,
            ),
            CompressionLevel::Default => (
                MatcherBackend::Dfast,
                self.base_slice_size,
                DFAST_DEFAULT_WINDOW_SIZE,
            ),
            CompressionLevel::Better => (
                MatcherBackend::Simple,
                self.base_slice_size,
                self.base_window_size,
            ),
            CompressionLevel::Best => (
                MatcherBackend::Simple,
                self.base_slice_size,
                self.base_window_size,
            ),
        }
    }
}

impl Matcher for MatchGeneratorDriver {
    fn reset(&mut self, level: CompressionLevel) {
        let (backend, slice_size, max_window_size) = self.level_config(level);
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
        match self.active_backend {
            MatcherBackend::Simple => {
                let vec_pool = &mut self.vec_pool;
                let suffix_pool = &mut self.suffix_pool;
                self.match_generator.max_window_size = max_window_size;
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

    fn window_size(&self) -> u64 {
        match self.active_backend {
            MatcherBackend::Simple => self.match_generator.max_window_size as u64,
            MatcherBackend::Dfast => {
                self.dfast_match_generator.as_ref().unwrap().max_window_size as u64
            }
        }
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
            MatcherBackend::Dfast => self
                .dfast_match_generator
                .as_ref()
                .unwrap()
                .get_last_space(),
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
                    .unwrap()
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
                .dfast_match_generator
                .as_mut()
                .unwrap()
                .start_matching(&mut handle_sequence),
        }
    }
    fn skip_matching(&mut self) {
        match self.active_backend {
            MatcherBackend::Simple => self.match_generator.skip_matching(),
            MatcherBackend::Dfast => self.dfast_match_generator.as_mut().unwrap().skip_matching(),
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
}

impl MatchGenerator {
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
        }
    }

    fn reset(&mut self, mut reuse_space: impl FnMut(Vec<u8>, SuffixStore)) {
        self.window_size = 0;
        #[cfg(debug_assertions)]
        self.concat_window.clear();
        self.suffix_idx = 0;
        self.last_idx_in_sequence = 0;
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

            // Look in each window entry
            let mut candidate = None;
            for (match_entry_idx, match_entry) in self.window.iter().enumerate() {
                let is_last = match_entry_idx == self.window.len() - 1;
                if let Some(match_index) = match_entry.suffixes.get(key) {
                    let match_slice = if is_last {
                        &match_entry.data[match_index..self.suffix_idx]
                    } else {
                        &match_entry.data[match_index..]
                    };

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
                self.add_suffixes_till(self.suffix_idx + match_len);

                // All literals that were not included between this match and the last are now included here
                let last_entry = self.window.last().unwrap();
                let literals = &last_entry.data[self.last_idx_in_sequence..self.suffix_idx];

                // Update the indexes, all indexes upto and including the current index have been included in a sequence now
                self.suffix_idx += match_len;
                self.last_idx_in_sequence = self.suffix_idx;
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
        Self::mismatch_chunks::<8>(a, b)
    }

    /// Find the common prefix length between two byte slices with a configurable chunk length
    /// This enables vectorization optimizations
    fn mismatch_chunks<const N: usize>(xs: &[u8], ys: &[u8]) -> usize {
        let off = core::iter::zip(xs.chunks_exact(N), ys.chunks_exact(N))
            .take_while(|(x, y)| x == y)
            .count()
            * N;
        off + core::iter::zip(&xs[off..], &ys[off..])
            .take_while(|(x, y)| x == y)
            .count()
    }

    /// Process bytes and add the suffixes to the suffix store up to a specific index
    #[inline(always)]
    fn add_suffixes_till(&mut self, idx: usize) {
        let last_entry = self.window.last_mut().unwrap();
        if last_entry.data.len() < MIN_MATCH_LEN {
            return;
        }
        let slice = &last_entry.data[self.suffix_idx..idx];
        for (key_index, key) in slice.windows(MIN_MATCH_LEN).enumerate() {
            if !last_entry.suffixes.contains_key(key) {
                last_entry.suffixes.insert(key, self.suffix_idx + key_index);
            }
        }
    }

    /// Skip matching for the whole current window entry
    fn skip_matching(&mut self) {
        let len = self.window.last().unwrap().data.len();
        self.add_suffixes_till(len);
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
            let current = self.window.back().unwrap().as_slice();
            handle_sequence(Sequence::Literals {
                literals: &current[literals_start..],
            });
        }
    }

    fn ensure_hash_tables(&mut self) {
        if self.short_hash.is_empty() {
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

    let assert_seq_equal = |seq1: Sequence<'_>, seq2: Sequence<'_>, reconstructed: &mut Vec<u8>| {
        assert_eq!(seq1, seq2);
        match seq2 {
            Sequence::Literals { literals } => reconstructed.extend_from_slice(literals),
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => {
                reconstructed.extend_from_slice(literals);
                let start = reconstructed.len() - offset;
                let end = start + match_len;
                reconstructed.extend_from_within(start..end);
            }
        }
    };

    matcher.add_data(
        alloc::vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[0, 0, 0, 0, 0],
                offset: 5,
                match_len: 5,
            },
            &mut reconstructed,
        )
    });

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

    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[1, 2, 3, 4, 5, 6],
                offset: 6,
                match_len: 6,
            },
            &mut reconstructed,
        )
    });
    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[],
                offset: 12,
                match_len: 6,
            },
            &mut reconstructed,
        )
    });
    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[],
                offset: 28,
                match_len: 5,
            },
            &mut reconstructed,
        )
    });
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 0, 0, 0, 0, 0],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 0, 0, 0, 0, 0]);

    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[],
                offset: 23,
                match_len: 6,
            },
            &mut reconstructed,
        )
    });
    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[7, 8, 9, 10, 11],
                offset: 16,
                match_len: 5,
            },
            &mut reconstructed,
        )
    });
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![0, 0, 0, 0, 0],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[0, 0, 0, 0, 0]);

    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[],
                offset: 5,
                match_len: 5,
            },
            &mut reconstructed,
        )
    });
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![7, 8, 9, 10, 11],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[7, 8, 9, 10, 11]);

    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[],
                offset: 15,
                match_len: 5,
            },
            &mut reconstructed,
        )
    });
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

    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[],
                offset: 5,
                match_len: 5,
            },
            &mut reconstructed,
        )
    });
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![0, 0, 11, 13, 15, 17, 20, 11, 13, 15, 17, 20, 21, 23],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[0, 0, 11, 13, 15, 17, 20, 11, 13, 15, 17, 20, 21, 23]);

    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[0, 0, 11, 13, 15, 17, 20],
                offset: 5,
                match_len: 5,
            },
            &mut reconstructed,
        )
    });
    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Literals {
                literals: &[21, 23],
            },
            &mut reconstructed,
        )
    });
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
