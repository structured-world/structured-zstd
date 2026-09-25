//! The reference's original dictionary trainer (`zdict.c`,
//! `ZDICT_trainFromBuffer_legacy`), which `zstd --train-legacy` runs.
//!
//! It walks the suffix array of the whole corpus and, for every position not
//! yet covered, measures how many other positions share a prefix of at least
//! [`MIN_MATCH_LENGTH`] bytes with it. A prefix repeated often enough becomes a
//! candidate segment, scored by the bytes it would save; overlapping candidates
//! merge, and the best of them, up to the requested size, are the dictionary.
//! Selectivity sets how often "often enough" is: a candidate needs at least
//! `samples >> selectivity` repetitions, and never fewer than [`MIN_RATIO`].

use alloc::vec;
use alloc::vec::Vec;

use super::suffix_array::suffix_array;

/// Fewest repetitions that make a prefix a candidate (`MINRATIO`).
const MIN_RATIO: u32 = 4;
/// Shortest prefix counted as a repetition (`MINMATCHLENGTH`).
const MIN_MATCH_LENGTH: usize = 7;
/// Longest repetition length told apart when scoring (`LLIMIT`).
const LENGTH_LIMIT: usize = 64;
/// Smallest candidate table (`DICTLISTSIZE_DEFAULT`).
const DICT_LIST_SIZE_DEFAULT: usize = 10_000;
/// Bytes of noise after the corpus, so a comparison running off its end stops
/// on a mismatch (`NOISELENGTH`).
const NOISE_LENGTH: usize = 32;
/// Most corpus the trainer reads; whole samples beyond it are dropped
/// (`ZDICT_MAX_SAMPLES_SIZE`).
const MAX_SAMPLES_SIZE: usize = 2000 << 20;
/// Smallest dictionary content the trainer returns (`ZDICT_CONTENTSIZE_MIN`).
const CONTENT_SIZE_MIN: usize = 128;
/// Smallest corpus worth training on (`ZDICT_MIN_SAMPLES_SIZE`).
pub(crate) const MIN_SAMPLES_SIZE: usize = CONTENT_SIZE_MIN * MIN_RATIO as usize;
/// Selectivity the reference uses when none is given (`g_selectivity_default`).
pub const DEFAULT_SELECTIVITY: u32 = 9;

/// Smallest dictionary the trainer is asked for (`ZDICT_DICTSIZE_MIN`).
pub(crate) const DICT_SIZE_MIN: usize = 256;

/// What was too small for the legacy trainer to produce a dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TooSmall {
    /// The requested size is below [`DICT_SIZE_MIN`].
    Dictionary,
    /// The corpus is smaller than [`MIN_SAMPLES_SIZE`].
    Corpus,
    /// The corpus repeats too little to fill [`CONTENT_SIZE_MIN`] bytes.
    Content,
}

/// A candidate segment (`dictItem`). In the table, entry 0 is a header whose
/// `pos` counts the entries in use, the header included, and whose `savings`
/// is the largest value, so the sorted insert stops on it.
#[derive(Debug, Clone, Copy, Default)]
struct DictItem {
    pos: u32,
    length: u32,
    savings: u32,
}

/// The corpus followed by its noise band, read through bounds-checked
/// accessors that treat anything past the band as a mismatch.
struct Corpus {
    bytes: Vec<u8>,
}

impl Corpus {
    fn new(samples: &[u8]) -> Self {
        let mut bytes = Vec::with_capacity(samples.len() + NOISE_LENGTH);
        bytes.extend_from_slice(samples);
        // The reference's noise generator (`ZDICT_fillNoise`).
        let mut acc: u32 = 2_654_435_761;
        for _ in 0..NOISE_LENGTH {
            acc = acc.wrapping_mul(2_246_822_519);
            bytes.push((acc >> 21) as u8);
        }
        Self { bytes }
    }

    #[inline]
    fn byte(&self, at: usize) -> Option<u8> {
        self.bytes.get(at).copied()
    }

    #[inline]
    fn read16(&self, at: usize) -> Option<u16> {
        let pair = self.bytes.get(at..at + 2)?;
        Some(u16::from_le_bytes([pair[0], pair[1]]))
    }

    #[inline]
    fn read64(&self, at: usize) -> Option<u64> {
        let word = self.bytes.get(at..at + 8)?;
        Some(u64::from_le_bytes(word.try_into().expect("eight bytes")))
    }

    /// Bytes `a` and `b` have in common (`ZDICT_count`), compared a word at
    /// a time as the reference compares them.
    #[inline]
    fn common(&self, a: usize, b: usize) -> usize {
        let bytes = self.bytes.as_slice();
        let Some(limit) = bytes.len().checked_sub(a.max(b)) else {
            return 0;
        };
        let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
        let mut n = 0;
        while n + 8 <= limit {
            let diff = word(a + n) ^ word(b + n);
            if diff != 0 {
                return n + (diff.trailing_zeros() / 8) as usize;
            }
            n += 8;
        }
        while n < limit && bytes[a + n] == bytes[b + n] {
            n += 1;
        }
        n
    }
}

/// The suffix array with one extra slot on each side, both pointing into the
/// noise band, as the reference lays it out (`suffix0[0]` and
/// `suffix[bufferSize]`): a walk off either end compares against noise and
/// stops there.
struct Suffixes {
    padded: Vec<u32>,
    noise: u32,
}

impl Suffixes {
    fn new(sa: Vec<u32>, len: u32) -> Self {
        let mut padded = Vec::with_capacity(sa.len() + 2);
        padded.push(len);
        padded.extend_from_slice(&sa);
        padded.push(len);
        Self { padded, noise: len }
    }

    /// `suffix[at]`, for `at` in `-1..=len`; any rank further out is the noise
    /// band as well, which a walk can only reach on a corpus whose tail matches
    /// the noise.
    #[inline]
    fn at(&self, at: i64) -> usize {
        // A walk stops at rank -1 at the lowest, where the padding holds the
        // noise position.
        debug_assert!(at >= -1);
        self.padded
            .get((at + 1) as usize)
            .copied()
            .unwrap_or(self.noise) as usize
    }
}

/// Train dictionary content from `samples`, the concatenation of samples whose
/// lengths are `sample_sizes`, keeping at most `dict_size` bytes.
///
/// The segments are laid out best last, so the most valuable content sits
/// nearest the data and is reached with the smallest offsets. `selectivity` of
/// zero is [`DEFAULT_SELECTIVITY`].
pub(crate) fn train_legacy_raw(
    samples: &[u8],
    sample_sizes: &[usize],
    dict_size: usize,
    selectivity: u32,
) -> Result<Vec<u8>, TooSmall> {
    let total: usize = sample_sizes.iter().sum();
    debug_assert_eq!(total, samples.len(), "the sizes describe the corpus");
    if dict_size < DICT_SIZE_MIN {
        return Err(TooSmall::Dictionary);
    }
    if total < MIN_SAMPLES_SIZE {
        return Err(TooSmall::Corpus);
    }
    let selectivity = if selectivity == 0 {
        DEFAULT_SELECTIVITY
    } else {
        selectivity
    };
    let nb_samples = sample_sizes.len();
    let min_rep = if selectivity > 30 {
        MIN_RATIO
    } else {
        // A repetition count past u32 is more than a u32-indexed corpus has
        // positions, so nothing could qualify.
        u32::try_from(nb_samples >> selectivity).map_err(|_| TooSmall::Content)?
    };
    let list_size = DICT_LIST_SIZE_DEFAULT.max(nb_samples).max(dict_size / 16);
    let mut list = vec![DictItem::default(); list_size];
    list[0] = DictItem {
        pos: 1,
        length: 0,
        savings: u32::MAX,
    };

    // Whole samples past the size limit are left out, last first.
    let mut used = total;
    let mut kept = nb_samples;
    while used > MAX_SAMPLES_SIZE {
        kept -= 1;
        used -= sample_sizes[kept];
    }
    let corpus = Corpus::new(&samples[..used]);
    find_segments(&mut list, &corpus, used, min_rep);

    let segments = list[0].pos as usize;
    let content_size: usize = list[1..segments].iter().map(|d| d.length as usize).sum();
    if content_size < CONTENT_SIZE_MIN {
        return Err(TooSmall::Content);
    }
    // Keep the best segments that fit, in rank order.
    let mut fit = 1;
    let mut size = 0usize;
    while fit < segments {
        let next = size + list[fit].length as usize;
        if next > dict_size {
            break;
        }
        size = next;
        fit += 1;
    }
    // Filled from the back, as the reference fills its buffer: rank 1 last.
    let mut content = vec![0u8; size];
    let mut end = size;
    for item in &list[1..fit] {
        let length = item.length as usize;
        let start = end - length;
        let from = item.pos as usize;
        content[start..end].copy_from_slice(&corpus.bytes[from..from + length]);
        end = start;
    }
    debug_assert_eq!(end, 0, "the kept segments fill the content exactly");
    Ok(content)
}

/// `ZDICT_trainBuffer_legacy`: walk every uncovered position of the corpus in
/// text order and insert the segment its suffix neighbourhood yields.
fn find_segments(list: &mut [DictItem], corpus: &Corpus, len: usize, min_rep: u32) {
    let min_ratio = min_rep.max(MIN_RATIO);
    let sa = suffix_array(&corpus.bytes[..len]);
    let mut rank = vec![0u32; len];
    for (at, &pos) in sa.iter().enumerate() {
        rank[pos as usize] = at as u32;
    }
    let suffixes = Suffixes::new(sa, len as u32);
    // Slack past the corpus, as the reference allocates it: a covered run may
    // be marked into the noise band.
    let mut done = vec![false; len + 16];
    let max_size = list.len() as u32;

    let mut cursor = 0usize;
    while cursor < len {
        if done[cursor] {
            cursor += 1;
            continue;
        }
        let solution = analyze_position(
            &mut done,
            &suffixes,
            i64::from(rank[cursor]),
            corpus,
            min_ratio,
        );
        if solution.length == 0 {
            cursor += 1;
            continue;
        }
        insert_item(list, max_size, solution, corpus);
        cursor += solution.length as usize;
    }
}

/// Mark `at` covered, if it lies inside the marks.
#[inline]
fn mark(done: &mut [bool], at: usize) {
    if let Some(slot) = done.get_mut(at) {
        *slot = true;
    }
}

/// Mark `len` positions from `at` covered, as far as the marks reach. Both
/// are bounded by the corpus length, so the sum cannot overflow.
#[inline]
fn mark_run(done: &mut [bool], at: usize, len: usize) {
    let end = (at + len).min(done.len());
    if at < end {
        done[at..end].fill(true);
    }
}

/// `ZDICT_analyzePos`: the segment the suffix of rank `start` leads to, or an
/// empty one. Every position the analysis settles is marked in `done`.
fn analyze_position(
    done: &mut [bool],
    suffixes: &Suffixes,
    mut start: i64,
    corpus: &Corpus,
    min_ratio: u32,
) -> DictItem {
    let mut pos = suffixes.at(start);
    let mut end = start;
    let empty = DictItem::default();
    mark(done, pos);

    // A run of one repeated pair is skipped whole: it compresses without a
    // dictionary.
    let repeats = |a: usize, b: usize| matches!((corpus.read16(a), corpus.read16(b)), (Some(x), Some(y)) if x == y);
    if repeats(pos, pos + 2) || repeats(pos + 1, pos + 3) || repeats(pos + 2, pos + 4) {
        let pattern = corpus.read16(pos + 4);
        let mut pattern_end = 6usize;
        while pattern.is_some() && corpus.read16(pos + pattern_end) == pattern {
            pattern_end += 2;
        }
        if corpus.byte(pos + pattern_end).is_some()
            && corpus.byte(pos + pattern_end) == corpus.byte(pos + pattern_end - 1)
        {
            pattern_end += 1;
        }
        mark_run(done, pos + 1, pattern_end - 1);
        return empty;
    }

    // The neighbours sharing at least the minimum length, forward then back.
    loop {
        end += 1;
        if corpus.common(pos, suffixes.at(end)) < MIN_MATCH_LENGTH {
            break;
        }
    }
    while corpus.common(pos, suffixes.at(start - 1)) >= MIN_MATCH_LENGTH {
        start -= 1;
    }

    if ((end - start) as u32) < min_ratio {
        for id in start..end {
            mark(done, suffixes.at(id));
        }
        return empty;
    }

    // Lengthen the shared prefix one byte at a time while the most common
    // continuation still repeats often enough.
    let mut refined_start = start;
    let mut refined_end = end;
    let mut mml = MIN_MATCH_LENGTH;
    loop {
        let mut current_char = 0u8;
        let mut current_count = 0u32;
        let mut current_id = refined_start;
        let mut selected_count = 0u32;
        let mut selected_id = current_id;
        for id in refined_start..refined_end {
            let c = corpus.byte(suffixes.at(id) + mml).unwrap_or(0);
            if c != current_char {
                if current_count > selected_count {
                    selected_count = current_count;
                    selected_id = current_id;
                }
                current_id = id;
                current_char = c;
                current_count = 0;
            }
            current_count += 1;
        }
        if current_count > selected_count {
            selected_count = current_count;
            selected_id = current_id;
        }
        if selected_count < min_ratio {
            break;
        }
        refined_start = selected_id;
        refined_end = refined_start + i64::from(selected_count);
        mml += 1;
    }

    // Measure the refined segment's neighbourhood.
    start = refined_start;
    pos = suffixes.at(refined_start);
    end = start;
    let mut lengths = [0u32; LENGTH_LIMIT];
    loop {
        end += 1;
        let length = corpus.common(pos, suffixes.at(end)).min(LENGTH_LIMIT - 1);
        lengths[length] += 1;
        if length < MIN_MATCH_LENGTH {
            break;
        }
    }
    {
        let mut length = MIN_MATCH_LENGTH;
        while length >= MIN_MATCH_LENGTH && start > 0 {
            length = corpus
                .common(pos, suffixes.at(start - 1))
                .min(LENGTH_LIMIT - 1);
            lengths[length] += 1;
            if length >= MIN_MATCH_LENGTH {
                start -= 1;
            }
        }
    }

    // The longest length still shared by enough neighbours.
    let mut cumulative = [0u32; LENGTH_LIMIT];
    cumulative[LENGTH_LIMIT - 1] = lengths[LENGTH_LIMIT - 1];
    for i in (0..LENGTH_LIMIT - 1).rev() {
        cumulative[i] = cumulative[i + 1] + lengths[i];
    }
    let mut max_length = LENGTH_LIMIT - 1;
    while max_length >= MIN_MATCH_LENGTH && cumulative[max_length] < min_ratio {
        max_length -= 1;
    }
    // Do not end inside a run of the last byte.
    {
        let last = corpus.byte(pos + max_length - 1);
        let mut l = max_length;
        while l >= 2 && corpus.byte(pos + l - 2) == last {
            l -= 1;
        }
        max_length = l;
    }
    if max_length < MIN_MATCH_LENGTH {
        return empty;
    }

    let mut savings = [0u32; LENGTH_LIMIT];
    for i in MIN_MATCH_LENGTH..=max_length {
        savings[i] = savings[i - 1].wrapping_add(lengths[i].wrapping_mul((i - 3) as u32));
    }
    let solution = DictItem {
        pos: pos as u32,
        length: max_length as u32,
        savings: savings[max_length],
    };

    for id in start..end {
        let tested = suffixes.at(id);
        let length = if tested == pos {
            max_length
        } else {
            corpus.common(pos, tested).min(max_length)
        };
        mark_run(done, tested, length);
    }
    solution
}

/// Whether the `length` bytes at `a` equal those at `b` (`isIncluded`).
fn is_included(corpus: &Corpus, a: usize, b: usize, length: usize) -> bool {
    match (
        corpus.bytes.get(a..a + length),
        corpus.bytes.get(b..b + length),
    ) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// Move entry `at` towards the front while its savings beat its predecessor's.
fn promote(list: &mut [DictItem], mut at: usize) -> usize {
    let item = list[at];
    while at > 1 && list[at - 1].savings < item.savings {
        list[at] = list[at - 1];
        at -= 1;
    }
    list[at] = item;
    at
}

/// `ZDICT_tryMerge`: fold `elt` into an entry it overlaps, skipping entry
/// `skip`. Returns the merged entry's index, or 0 when nothing merged.
fn try_merge(list: &mut [DictItem], elt: DictItem, skip: usize, corpus: &Corpus) -> usize {
    let size = list[0].pos as usize;
    let elt_end = elt.pos + elt.length;

    // An existing entry starts inside `elt`: extend it backwards.
    for u in 1..size {
        if u == skip {
            continue;
        }
        if list[u].pos > elt.pos && list[u].pos <= elt_end {
            let added = list[u].pos - elt.pos;
            list[u].length += added;
            list[u].pos = elt.pos;
            list[u].savings = list[u]
                .savings
                .wrapping_add(elt.savings.wrapping_mul(added) / elt.length);
            list[u].savings = list[u].savings.wrapping_add(elt.length / 8);
            return promote(list, u);
        }
    }

    // `elt` starts inside an existing entry, or right after a copy of it.
    for u in 1..size {
        if u == skip {
            continue;
        }
        if list[u].pos + list[u].length >= elt.pos && list[u].pos < elt.pos {
            let added = elt_end as i64 - i64::from(list[u].pos + list[u].length);
            list[u].savings = list[u].savings.wrapping_add(elt.length / 8);
            if added > 0 {
                list[u].length += added as u32;
                list[u].savings = list[u]
                    .savings
                    .wrapping_add(elt.savings.wrapping_mul(added as u32) / elt.length);
            }
            return promote(list, u);
        }
        let head = corpus.read64(list[u].pos as usize);
        if head.is_some()
            && head == corpus.read64(elt.pos as usize + 1)
            && is_included(
                corpus,
                list[u].pos as usize,
                elt.pos as usize + 1,
                list[u].length as usize,
            )
        {
            // The reference takes this product at pointer width, where it does
            // not wrap, unlike the two above.
            let added = (i64::from(elt.length) - i64::from(list[u].length)).max(1) as u64;
            list[u].pos = elt.pos;
            list[u].savings = list[u]
                .savings
                .wrapping_add((u64::from(elt.savings) * added / u64::from(elt.length)) as u32);
            list[u].length = elt.length.min(list[u].length + 1);
            return u;
        }
    }
    0
}

/// `ZDICT_removeDictItem`.
fn remove_item(list: &mut [DictItem], id: usize) {
    if id == 0 {
        return;
    }
    let max = list[0].pos as usize;
    for u in id..max - 1 {
        list[u] = list[u + 1];
    }
    list[0].pos -= 1;
}

/// `ZDICT_insertDictItem`: merge `elt` into the table if it overlaps an entry,
/// and keep merging while the merged entry overlaps another; otherwise insert
/// it in savings order, dropping the last entry when the table is full.
fn insert_item(list: &mut [DictItem], max_size: u32, elt: DictItem, corpus: &Corpus) {
    let mut merge_id = try_merge(list, elt, 0, corpus);
    if merge_id != 0 {
        loop {
            let merged = try_merge(list, list[merge_id], merge_id, corpus);
            if merged == 0 {
                return;
            }
            remove_item(list, merge_id);
            merge_id = merged;
        }
    }
    let next = list[0].pos.min(max_size - 1) as usize;
    let mut current = next - 1;
    while list[current].savings < elt.savings {
        list[current + 1] = list[current];
        current -= 1;
    }
    list[current + 1] = elt;
    list[0].pos = next as u32 + 1;
}

#[cfg(test)]
mod tests;
