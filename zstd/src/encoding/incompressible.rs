use super::CompressionLevel;
use crate::common::MAX_BLOCK_SIZE;
use alloc::vec;
use alloc::vec::Vec;
use core::mem::MaybeUninit;

/// What the block's own bytes cannot answer: has this content been seen
/// earlier in the frame?
///
/// The classifier below decides from the block in hand, so a block that is
/// incompressible within itself but duplicates an earlier one looks exactly
/// like noise and, taken alone, would be written off unsearched — throwing
/// away a match the size of the duplicate. This records a fingerprint at every
/// grid position of every block that reaches the decision, and reports whether
/// the block in hand collides with one; a collision sends it to the search.
///
/// Recording lands on a fixed grid of stream offsets and probing sweeps a run
/// of CONSECUTIVE positions, which is what makes a shifted duplicate findable
/// without reading the whole block: a copy sits some distance from its
/// original, the probed positions map to original positions that far lower, and
/// among as many consecutive values as the record step exactly one is a
/// multiple of it, so exactly one probe meets a recorded key, whatever the
/// distance is. Probing on the same grid it records on would instead see a
/// repeat only at distances that happen to be a multiple of the step, and miss,
/// say, a block that repeats the previous one after two inserted bytes.
///
/// Each slot carries the frame offset it was recorded at, so a fingerprint is
/// consulted only while the matcher could still reach that far back and expires
/// on its own. Clearing the whole table on a window boundary instead would
/// forget content that is still in reach — a block-sized window would drop the
/// block a duplicate is about to match against.
///
/// A collision that is merely a hash coincidence costs a search, never
/// correctness, and at a 32-bit fingerprint over a few thousand live entries
/// that is rarer than one frame in a million.
#[derive(Debug, Default)]
pub(crate) struct SeenContentGrid {
    /// A slot belongs to the frame whose `epoch` it carries; one from an
    /// earlier frame reads as empty, which is what makes starting a frame free.
    ///
    /// Packed into a plain integer rather than held as [`SeenSample`] so that
    /// taking the table is a zeroed allocation: `vec![0u64; n]` asks the
    /// allocator for zeroed memory, which a large request gets as pages the
    /// kernel has not had to write, while `vec![Struct::default(); n]` writes
    /// every element. A compressor is built per frame in the shape this is for,
    /// so that write was a whole table memset per frame — on a mebibyte of
    /// patterned input at the fast levels it was most of the gap against the
    /// release before this one.
    slots: Vec<u64>,
    /// One byte of each slot's key, in a table of its own. A probe run is 512
    /// lookups and nearly all of them miss, so the miss has to be cheap: a byte
    /// per slot keeps a window's worth of them in cache where the same number of
    /// sixteen-byte samples does not, and only a byte that matches is worth
    /// reading the sample for. Never zero for a live record, so a slot no frame
    /// has written cannot match.
    tags: Vec<u8>,
    /// Which frame the live slots belong to. Starts at 0 with a freshly
    /// allocated (all-zero) table, and every frame increments it first, so no
    /// frame ever runs under the epoch a zeroed slot carries.
    epoch: u16,
    /// Bytes of this frame recorded so far. `u64` on every target: a frame can
    /// be longer than a 16- or 32-bit address space, and only this counter
    /// would notice.
    frame_offset: u64,
    /// Frame offset up to which the search stays on after a hit.
    repeat_until: u64,
    /// Whether this frame has asked the grid anything yet.
    ///
    /// Until it has, recording a searched block is work for an answer no one
    /// will read: only a block the classifier calls incompressible ever probes,
    /// and a frame where that never happens — structured text, a log stream,
    /// anything the matcher codes well — would otherwise take the table and hash
    /// a key every [`Self::RECORD_STEP`] bytes of every block for nothing. On a
    /// mebibyte of patterned input at the fast levels that was measurable twice
    /// over: the hashing itself, and the table it takes per frame, which is
    /// enough to move the allocator off the fresh pages a much larger matcher
    /// table was getting for free and onto zeroing a recycled one.
    ///
    /// What it gives up is a duplicate of a block searched BEFORE the frame's
    /// first incompressible one. A duplicate of a block that compressed well is
    /// itself compressible, so it is not at risk of the skip in the first place.
    asked: bool,
}

/// One slot, unpacked from the eight bytes it is stored in. Widening any field
/// costs a byte of table for every slot, and takes the packed form past a word.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SeenSample {
    /// Sixteen bits here and eight more in the tag table: a coincidence costs a
    /// search that finds nothing, never a wrong answer, and at twenty-four bits
    /// over the few thousand probes a frame makes that is one frame in
    /// thousands.
    fingerprint: u16,
    epoch: u16,
    /// The record's offset in units of [`SeenContentGrid::RECORD_STEP`], which
    /// is what every record sits on. A frame would have to run past two
    /// tebibytes for this to be too narrow.
    at_step: u32,
}

impl SeenSample {
    /// Fingerprint in the low sixteen bits, epoch above it, offset in the high
    /// half. A zeroed word is epoch 0, which no frame ever runs under, so an
    /// untouched slot reads as empty without being written first.
    #[inline]
    fn pack(self) -> u64 {
        u64::from(self.fingerprint)
            | (u64::from(self.epoch) << 16)
            | (u64::from(self.at_step) << 32)
    }

    #[inline]
    fn unpack(word: u64) -> Self {
        Self {
            fingerprint: word as u16,
            epoch: (word >> 16) as u16,
            at_step: (word >> 32) as u32,
        }
    }
}

impl SeenContentGrid {
    /// Ceiling on the table. Sized so a window's worth of records fits without
    /// evicting: a probe run meets exactly ONE recorded key, so an evicted
    /// record is a repeat missed outright, where the previous scheme had a
    /// couple of hundred chances per block and could afford to lose most of
    /// them. At nine bytes a slot (the packed sample and its tag) this is
    /// 576 KiB, taken only by a frame whose window holds that many records.
    const SLOTS: usize = 64 * 1024;
    /// Floor on the table, so a tiny window still has room for a few anchors
    /// without a slot collision reading as a repeat on every one.
    const MIN_SLOTS: usize = 64;
    /// Bytes read per sample.
    const KEY_LEN: usize = 8;
    /// Stream offsets that get recorded: every one that is a multiple of this,
    /// in the frame's own coordinates rather than the block's, so the same
    /// content lands on the same offsets however the blocks are cut. 256 records
    /// per 128 KiB block, and a 4 MiB window's worth fits the table without
    /// evicting most of itself.
    ///
    /// A record is a random write into a table as wide as the window's records,
    /// so the step is what keeps that table in cache. Records every 128 bytes,
    /// with the table four times the size, cost 33-42% of the encode of a
    /// mebibyte of noise at the fast levels and dfast on x86_64.
    const RECORD_STEP: usize = 512;
    /// Consecutive positions probed per run. Equal to [`Self::RECORD_STEP`] by
    /// construction, not by coincidence: among that many consecutive stream
    /// offsets exactly one is a multiple of the step, so a copy at ANY distance
    /// from its original has exactly one probe that meets a recorded key.
    const PROBE_RUN: usize = Self::RECORD_STEP;
    /// A probe run starts every this fraction of a block, so a copy the grid
    /// misses is shorter than that fraction plus one run, wherever in the block
    /// it begins. A run answers a copy at any distance but only one that it
    /// begins inside, so with runs only at the start and the middle a block
    /// carrying a copy of its own content between them went out raw with the
    /// match in it.
    ///
    /// A quarter rather than an eighth: on a mebibyte of noise at levels -7 to
    /// 3 on x86_64, eight runs a block cost 13-19% over the two-run placement
    /// and four runs cost 9-12% less than eight, three interleaved runs of
    /// prebuilt binaries each. A copy a quarter of a block long still codes
    /// most of what the eighth would have found.
    const PROBE_FRACTION: usize = 4;
    /// What a rebase keeps: the widest window the format admits, so a record
    /// dropped there was out of every matcher's reach already.
    const REBASE_RETAIN_BYTES: u64 = 1 << 31;
    /// How far past a hit the search stays on: two maximum blocks, so an
    /// isolated miss inside repeating content cannot cost a block, while a frame
    /// that stops repeating returns to skipping within a block or two.
    const STICKY_REACH: u64 = 2 * MAX_BLOCK_SIZE as u64;
    /// Start a frame. Every frame calls this, most never consult the table, and
    /// clearing it here cost a 64 KiB fill per frame — a fixed few microseconds
    /// that a small frame pays in full. Stepping the epoch retires the previous
    /// frame's slots instead, and the table is allocated only once a frame
    /// actually records into it.
    pub(crate) fn reset_for_frame(&mut self) {
        self.retire_for_rebase();
    }

    /// Move the origin forward, keeping every record the matcher could still
    /// reach and dropping the rest.
    ///
    /// A frame that runs past what the step index holds has to start counting
    /// again, and retiring the whole table there costs a window's worth of
    /// blocks: each finds nothing on the grid and goes out raw although the
    /// matcher still holds and has indexed its original. What is kept is the
    /// last [`Self::REBASE_RETAIN_BYTES`], which covers the widest window the
    /// format admits, so nothing droppable was reachable anyway.
    ///
    /// Out of line and cold: this walks the whole table and runs once per couple
    /// of tebibytes of stream, while its callers run per block. Letting it inline
    /// into the caller that skips a block cost 22% of the encode of a repeated
    /// log stream at level 1 — 807-814 us against 983-986 us — for a body that
    /// never executes there.
    #[cold]
    #[inline(never)]
    fn rebase_offsets(&mut self) {
        let step = Self::RECORD_STEP as u64;
        let retain_steps = Self::REBASE_RETAIN_BYTES / step;
        let Some(base_steps) = (self.frame_offset / step).checked_sub(retain_steps) else {
            // Not far enough along to have anything to drop, which the caller's
            // guard makes unreachable — the origin only moves at the index
            // limit, and that is far past the retained span.
            return;
        };
        // Every advance rebases as it crosses, so the origin is never more than
        // a block past the index and this conversion always fits. Clamping
        // rather than asserting keeps a caller that somehow arrives further
        // along correct instead of panicking: a base beyond the index is older
        // than every record, which is exactly what the clamp then drops.
        let base_steps32 = u32::try_from(base_steps).unwrap_or(u32::MAX);
        for (word, tag) in self.slots.iter_mut().zip(self.tags.iter_mut()) {
            let mut held = SeenSample::unpack(*word);
            if held.epoch != self.epoch {
                continue;
            }
            match held.at_step.checked_sub(base_steps32) {
                Some(rebased) => {
                    held.at_step = rebased;
                    *word = held.pack();
                }
                // Older than the widest window: no probe could have used it.
                None => {
                    *word = 0;
                    *tag = 0;
                }
            }
        }
        self.frame_offset -= base_steps * step;
        self.repeat_until = self.repeat_until.saturating_sub(base_steps * step);
    }

    /// Retire every record and start the offsets again from zero.
    ///
    /// The frame boundary, where nothing recorded is reachable any more.
    fn retire_for_rebase(&mut self) {
        // The wrap lands on 0, which is the epoch a freshly allocated slot
        // carries, so the table is cleared on that one frame in sixty-five
        // thousand rather than letting a stale slot read as live.
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.slots.fill(0);
            self.tags.fill(0);
            self.epoch = 1;
        }
        self.frame_offset = 0;
        self.repeat_until = 0;
        self.asked = false;
    }

    pub(crate) fn heap_size(&self) -> usize {
        self.slots.capacity() * core::mem::size_of::<u64>() + self.tags.capacity()
    }

    /// The eight bytes at `at`.
    ///
    /// # Safety
    ///
    /// `at + KEY_LEN` must be within the block `block_ptr` points into. Reading
    /// through the pointer rather than slicing is what keeps the run cheap: a
    /// probe run is [`Self::PROBE_RUN`] consecutive positions, and the slice
    /// form pays a bounds check and a panic path at each of them for a bound the
    /// loop already holds.
    #[inline]
    unsafe fn key_at(block_ptr: *const u8, at: usize) -> u64 {
        // SAFETY: the caller guarantees `at + KEY_LEN` is inside the block, and
        // an unaligned read is what the byte-oriented key needs.
        unsafe { block_ptr.add(at).cast::<u64>().read_unaligned() }.to_le()
    }

    /// The slot a key belongs in and its tag, laid out as the full mix lays
    /// them out: the slot from the high half, the tag from bits 16 to 23.
    ///
    /// One fold and one multiply, because this is the line nearly every probe
    /// executes: a run is [`Self::PROBE_RUN`] positions, a block takes a run
    /// every [`Self::PROBE_FRACTION`] of it, and on noise every one misses. The
    /// fold carries the key's high bits down before the multiply, which alone
    /// would leave its top byte out of the slot. The fingerprint a hit is
    /// confirmed against comes from [`Self::avalanche`], an independent mix,
    /// and only a probe whose tag matches computes it.
    #[inline]
    fn placement(key: u64) -> u64 {
        let product = (key ^ (key >> 29)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        ((product >> 8) & 0xFFFF_FFFF_0000_0000) | ((product >> 40) & 0x00FF_0000)
    }

    /// The slot and tag of `key` in a table of `mask + 1` slots. Neither the
    /// tag nor the fingerprint is ever zero, so a slot no frame has written
    /// cannot read as a match.
    #[inline]
    fn slot_and_tag(key: u64, mask: usize) -> (usize, u8) {
        let placed = Self::placement(key);
        ((placed >> 32) as usize & mask, (placed >> 16) as u8 | 1)
    }

    #[inline]
    fn fingerprint(key: u64) -> u16 {
        (Self::avalanche(key) as u16) | 1
    }

    /// Whether the content at `at` was recorded within `reach`. Reads only: the
    /// probe run sweeps consecutive positions, and recording every one of them
    /// would fill the table with keys no later probe can align with.
    ///
    /// # Safety
    ///
    /// As [`Self::key_at`]: `at + KEY_LEN` must be inside the block.
    #[inline]
    unsafe fn probe_key(&self, block_ptr: *const u8, at: usize, reach: u64, mask: usize) -> bool {
        // SAFETY: the caller's bound, forwarded.
        let key = unsafe { Self::key_at(block_ptr, at) };
        let (slot, tag) = Self::slot_and_tag(key, mask);
        // The byte first: this is the only line nearly every probe executes.
        // SAFETY: `mask` is `len - 1` of both tables, which are the same power-of
        // -two length, so a masked slot is in bounds for either.
        if unsafe { *self.tags.get_unchecked(slot) } != tag {
            return false;
        }
        let held = SeenSample::unpack(unsafe { *self.slots.get_unchecked(slot) });
        let here = self.frame_offset + at as u64;
        if held.epoch != self.epoch || held.fingerprint != Self::fingerprint(key) {
            return false;
        }
        let recorded = u64::from(held.at_step) * Self::RECORD_STEP as u64;
        // A record is never ahead of a probe that meets it: records for a run go
        // in before the run, and both walk the block forwards. The subtraction
        // is checked all the same — the alternative is a wrap in release that
        // reads as a repeat at a distance of eighteen exabytes.
        debug_assert!(recorded <= here, "a record ahead of the probe that met it");
        here.checked_sub(recorded)
            .is_some_and(|apart| apart <= reach)
    }

    /// Put the content at `at` in the table, dated here.
    ///
    /// Written whether or not the slot was occupied by the same content: leaving
    /// a slot dated to the FIRST occurrence of a repeating run makes the third
    /// block measure its distance from there, which a one-block window reads as
    /// out of reach even though the block right behind it is exactly what the
    /// matcher would find — every other block of the run would go out raw.
    ///
    /// # Safety
    ///
    /// As [`Self::key_at`]: `at + KEY_LEN` must be inside the block.
    #[inline]
    unsafe fn record_key(&mut self, block_ptr: *const u8, at: usize, mask: usize) {
        // SAFETY: the caller's bound, forwarded.
        let key = unsafe { Self::key_at(block_ptr, at) };
        let (slot, tag) = Self::slot_and_tag(key, mask);
        let fingerprint = Self::fingerprint(key);
        // SAFETY: a masked slot is in bounds for both tables; see `probe_key`.
        unsafe {
            *self.tags.get_unchecked_mut(slot) = tag;
        }
        let here = self.frame_offset + at as u64;
        debug_assert!(
            here.is_multiple_of(Self::RECORD_STEP as u64),
            "records sit on the grid, which is what lets the offset be stored in steps",
        );
        let packed = SeenSample {
            fingerprint,
            epoch: self.epoch,
            at_step: (here / Self::RECORD_STEP as u64) as u32,
        }
        .pack();
        // SAFETY: a masked slot is in bounds for both tables; see `probe_key`.
        unsafe {
            *self.slots.get_unchecked_mut(slot) = packed;
        }
    }

    /// Record this block on the grid and report whether it duplicates content
    /// still within `window_size` of here.
    ///
    /// Recording happens whatever the answer: a block that goes out raw is still
    /// content a later block may duplicate, and one that gets searched is
    /// indexed by the matcher but only for as long as the window holds it.
    /// `window_size` is that reach; pass `0` when it is unknown, which keeps
    /// every record for the frame.
    pub(crate) fn record_and_report_repeat(&mut self, block: &[u8], window_size: usize) -> bool {
        self.asked = true;
        self.take_block(block, window_size, true)
    }

    /// Record a block the caller is going to SEARCH, without asking whether it
    /// repeats.
    ///
    /// The matcher indexes what it searches, so that content stays findable for
    /// as long as the window holds it — but only a block the grid recorded can
    /// send a later block to the search in the first place. Leaving searched
    /// blocks unrecorded means a later block made mostly of one of them looks
    /// like noise, finds nothing, and goes out raw with an almost block-sized
    /// match sitting in history. The probe is what costs; recording is a key
    /// every `RECORD_STEP` bytes.
    ///
    /// Nothing is recorded until the frame has asked the grid something at least
    /// once (see [`Self::asked`]); the offset still advances, so what follows
    /// keeps its true distance in the stream.
    pub(crate) fn record_searched(&mut self, block: &[u8], window_size: usize) {
        if !self.asked {
            self.skip_block(block.len());
            return;
        }
        self.take_block(block, window_size, false);
    }

    /// Record a block the caller searches without asking the classifier (see
    /// [`raw_skip_worth_asking`]), and count the frame as using the grid.
    ///
    /// Such a block may well be noise, so the reason [`Self::asked`] lets a
    /// searched block go unrecorded does not hold for it: a later block made of
    /// its copy among unique noise reads as noise to the classifier, and only a
    /// record of this block can send it to the search.
    pub(crate) fn record_unclassified(&mut self, block: &[u8], window_size: usize) {
        self.asked = true;
        self.take_block(block, window_size, false);
    }

    /// Advance past a block the caller has decided not to record, keeping every
    /// later record at its true distance in the stream.
    pub(crate) fn skip_recording(&mut self, len: usize) {
        self.skip_block(len);
    }

    /// Advance past a block the grid records nothing for, keeping the origin
    /// inside what the step index can express.
    ///
    /// Two paths reach here — a block before the frame's first probe, and one
    /// shorter than a key — and neither enters the walk that moves the origin.
    /// Left alone, a frame made only of such blocks would carry an origin the
    /// first walk after them cannot narrow, which is a panic rather than a lost
    /// repeat.
    ///
    /// The origin moves through [`Self::rebase_offsets`], the same way the walk
    /// does, and BEFORE the advance so the offset never leaves the index. Moving
    /// the origin alone would be cheaper and is wrong: a record dated in the old
    /// coordinates then sits AHEAD of everything after it, so the next probe
    /// measures a negative distance, reads no repeat, and a duplicate the
    /// matcher still holds goes out raw — and the sticky range would keep the
    /// search on for most of an index span.
    #[inline]
    fn skip_block(&mut self, len: usize) {
        let step = Self::RECORD_STEP as u64;
        if (self.frame_offset + len as u64) / step > u64::from(u32::MAX) {
            self.rebase_offsets();
        }
        self.frame_offset += len as u64;
    }

    fn take_block(&mut self, block: &[u8], window_size: usize, probe: bool) -> bool {
        // Too short to key on. The offset still advances, so the ages of what
        // follows stay true distances in the stream.
        if block.len() < Self::KEY_LEN {
            self.skip_block(block.len());
            return false;
        }
        let wanted = Self::slots_for(window_size);
        if self.slots.len() < wanted {
            // Both zeroed allocations, which the allocator can serve as pages
            // the kernel has not written; see the field's own note.
            self.slots = vec![0u64; wanted];
            self.tags = vec![0u8; wanted];
            // Zeroed slots carry epoch 0, so a frame must never run under it.
            self.epoch = self.epoch.max(1);
        }
        // The width of the table as it stands, not the width this frame's window
        // asks for. The table grows and is kept, so a compressor that once saw a
        // wide window spreads a later small frame's keys across all of it — which
        // looks like a defect and measures as the opposite. Masking at the
        // frame's own width instead, on one compressor with a wide frame first
        // and small ones after, three interleaved readings a side: 1 KiB frames
        // 93.8-94.3 M cycles against 97.9-99.1 M, 10 KiB frames 144.1-144.5 M
        // against 148.8-149.7 M, with instructions equal to five digits either
        // way. Narrow means the record walk keeps rewriting the same few cache
        // lines; wide gives each write its own.
        let mask = self.slots.len() - 1;
        let reach = if window_size == 0 {
            u64::MAX
        } else {
            window_size as u64
        };
        let mut repeat = false;
        let last = block.len() - Self::KEY_LEN;
        // Taken once for the whole block. Every position this walk reads is at
        // most `last`, which is the bound `key_at` needs, and the block is
        // borrowed for the call — so the two walks below can read through it
        // without re-deriving the same bound per position.
        let block_ptr = block.as_ptr();
        // Neither side reads the whole block. Recording lands on a FIXED grid in
        // the frame's own coordinates — every `RECORD_STEP` bytes of stream, so
        // the same content recorded once is recorded at the same stream offsets
        // however the blocks around it are cut. Probing takes `RECORD_STEP`
        // CONSECUTIVE positions, and that is what makes any shift work: a copy
        // sits at some distance D from its original, the probed positions map to
        // original positions D lower, and among `RECORD_STEP` consecutive values
        // exactly one is a multiple of `RECORD_STEP` — so exactly one probe
        // meets a recorded key, whatever D is.
        //
        // The pass this replaces read every byte looking for content-defined
        // anchors. It was correct and it was the cost: on a fast level a whole
        // extra pass over the block doubles the encode of incompressible input,
        // where the raw path is little more than a copy.
        //
        // Runs at every `PROBE_FRACTION` of the block: the one at the start
        // answers a duplicate of anything recorded earlier, and every later one
        // a block that carries a copy of its own earlier content, which reads as
        // incompressible by any sample of it and is a match the size of the copy
        // if the search runs. A run only answers a copy that it begins inside, so
        // the spacing bounds what a copy has to be to hide.
        //
        // Every run is also another independent chance for the one aligned probe
        // to meet a record that an earlier run's slot has since been written
        // over. Dropping the second of two runs on every block after a frame's
        // first cost ratio: four megabytes repeated at a shifted distance went
        // from 4,129,240 bytes to 4,194,762 at level 17.
        let step = Self::RECORD_STEP as u64;
        // A full run covers every distance a copy could sit at. On a block of a
        // couple of kilobytes it is a large share of the block, and the grid
        // then costs more than the duplicate it could find is worth — a missed
        // one there is bounded by the block. So the run is capped at a probe per
        // sixteen bytes, and the runs are spaced no closer than sixteen runs
        // apart, which keeps a small block's probes to a sixteenth of it.
        let run = Self::PROBE_RUN.min((block.len() / 16).max(8));
        let spacing = (block.len() / Self::PROBE_FRACTION).max(16 * run);
        // A record's offset is stored in grid steps, so a frame that runs past
        // what that index can hold moves its origin rather than wraps — a
        // wrapped offset reads as being near the start of the frame, and a
        // duplicate right behind it is then measured as out of the window and
        // written off. The move keeps everything a window could still reach and
        // costs one pass over the table per couple of tebibytes of stream.
        if (self.frame_offset + last as u64) / step > u64::from(u32::MAX) {
            self.rebase_offsets();
        }
        let mut abs = self.frame_offset.next_multiple_of(step);
        let block_end = self.frame_offset + last as u64;
        let runs = block.len().div_ceil(spacing);
        for idx in 0..runs {
            let start = idx * spacing;
            // Records for everything before this run go in FIRST, because
            // meeting them is the run's whole job — a block whose own first half
            // is the original is invisible to a run that probes before that half
            // is recorded. Strictly before: a grid point recorded at a position
            // the run then probes answers itself, and every block reads as its
            // own repeat.
            let until = self.frame_offset + start as u64;
            while abs < until && abs <= block_end {
                let at = (abs - self.frame_offset) as usize;
                // SAFETY: `abs <= block_end` is `at <= last`, and `last` is
                // `block.len() - KEY_LEN`.
                unsafe { self.record_key(block_ptr, at, mask) };
                abs += step;
            }
            // The start run on a frame's first block cannot hit anything: the
            // table is empty until that block records into it, and a frame of a
            // few kilobytes is one block. The later runs stay even on a frame's
            // only block: a block that copies its own content reads as noise to
            // every sample the classifier takes, and without them it goes out
            // raw with the match inside it. They are the one check between the
            // skip and that loss.
            // Nothing to ask once the answer is in: a probe is read-only and
            // the run reports one bool, so every lookup after the first hit is
            // a random table access for a verdict already reached.
            //
            // Skipping the runs entirely while the sticky range already forces
            // the search was tried on top of this and is not worth it: on eight
            // mebibytes repeating at a shifted distance — the shape the sticky
            // range exists for — it took 1.9 M instructions off 6,555 M and left
            // cycles higher in all three interleaved pairs. This early exit has
            // already taken that saving, because a block that duplicates the one
            // before it is answered in the first few probes. And it would cost
            // something real: a hit inside the range EXTENDS it, so a run that
            // never happens lets the range lapse where today it grows.
            if probe && !repeat && (self.frame_offset != 0 || start != 0) {
                let end = (start + run).min(last + 1);
                for at in start..end {
                    // SAFETY: `end <= last + 1`, so every `at` here is at most
                    // `last`, which is `block.len() - KEY_LEN`.
                    if unsafe { self.probe_key(block_ptr, at, reach, mask) } {
                        repeat = true;
                        break;
                    }
                }
            }
            // The rest of the block, once no run is left to probe it.
            if idx + 1 == runs {
                while abs <= block_end {
                    let at = (abs - self.frame_offset) as usize;
                    // SAFETY: as the record walk above — `at <= last`.
                    unsafe { self.record_key(block_ptr, at, mask) };
                    abs += step;
                }
            }
        }
        self.frame_offset += block.len() as u64;
        // Content that repeats does not repeat in every block, and the sampling
        // can miss one the matcher would have found — a miss costs a whole block
        // written out raw, so a hit keeps the search on for a stretch after it.
        // The case the skip exists for, a frame of noise, never enters this: it
        // never hits.
        if repeat {
            self.repeat_until = self.frame_offset + Self::STICKY_REACH;
        }
        repeat || self.frame_offset <= self.repeat_until
    }

    /// How many slots a window's worth of anchors needs, rounded up to a power
    /// of two and held to [`Self::SLOTS`].
    ///
    /// The window is the reach: a fingerprint older than that is never
    /// consulted, so a table wider than the anchors the window can hold is
    /// memory a frame allocates and faults for nothing. A kibibyte frame was
    /// taking a 64 KiB table for the handful of anchors it could ever record,
    /// which on the cheapest levels was a third of the encode. Four slots per
    /// anchor keeps eviction rare.
    fn slots_for(window_size: usize) -> usize {
        if window_size == 0 {
            return Self::SLOTS;
        }
        let records = (window_size / Self::RECORD_STEP).max(1) as u64;
        let wanted = (records * 4).next_power_of_two();
        (wanted as usize).clamp(Self::MIN_SLOTS, Self::SLOTS)
    }

    /// Full 64-bit avalanche (splitmix64's finalizer, or a two-word equivalent
    /// on a 32-bit machine): every output bit depends on every input bit, which
    /// a single multiply does not give — its low half is barely mixed.
    ///
    /// Only the fingerprint comes from it, which a probe computes once its tag
    /// matches. Cut from the same word as the slot and tag, one multiply with a
    /// fold does not hold: a weaker mix correlates the three, and a repeat that
    /// the grid used to report went unrecognised — the chain-finder regression
    /// test fails on it. Taken from an independent mix, the fingerprint still
    /// confirms what the cheap [`Self::placement`] only locates.
    #[inline]
    fn avalanche(key: u64) -> u64 {
        #[cfg(not(all(target_pointer_width = "32", not(target_family = "wasm"))))]
        {
            Self::avalanche_wide(key)
        }
        // A 32-bit machine has no 64-bit multiply, so each of the wide mix's
        // three becomes three of its own plus the carries; wasm32 is left on
        // the wide mix because its `i64.mul` is native.
        #[cfg(all(target_pointer_width = "32", not(target_family = "wasm")))]
        {
            Self::avalanche_narrow(key)
        }
    }

    #[cfg(any(
        test,
        not(all(target_pointer_width = "32", not(target_family = "wasm")))
    ))]
    #[inline]
    fn avalanche_wide(key: u64) -> u64 {
        let mut z = key.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// The same contract from five 32-bit multiplies: the low word, which the
    /// fingerprint and tag are cut from, mixes both halves of the key, and the
    /// high word, which the slot is cut from, is that word mixed again with the
    /// key's high half, so the slot and the bits checked against it are
    /// separate avalanches rather than one word read twice.
    #[cfg(any(test, all(target_pointer_width = "32", not(target_family = "wasm"))))]
    #[inline]
    fn avalanche_narrow(key: u64) -> u64 {
        // murmur3's 32-bit finaliser.
        fn fmix32(mut h: u32) -> u32 {
            h ^= h >> 16;
            h = h.wrapping_mul(0x85EB_CA6B);
            h ^= h >> 13;
            h = h.wrapping_mul(0xC2B2_AE35);
            h ^ (h >> 16)
        }
        let lo = key as u32;
        let hi = (key >> 32) as u32;
        let low = fmix32(lo ^ hi.wrapping_mul(0x9E37_79B1));
        let high = fmix32(low ^ hi ^ 0x27D4_EB2F);
        (u64::from(high) << 32) | u64::from(low)
    }
}

/// Block sizes at which a frame that stores its literals raw searches rather
/// than asks the classifier.
///
/// With literals stored raw (the fast strategy with a positive target length,
/// the negative levels) a search that finds nothing costs one pass of the fast
/// kernel plus the raw fallback, which is also all upstream does there. The
/// classifier's cost is fixed per block, so on a small block it is the larger
/// of the two. Measured with the classifier on and off in one binary, per
/// frame of noise, levels -7 to -1: from 2 KiB to 24 KiB searching was 1-57%
/// faster in all but two of the seventy cells on x86_64 and i686, at 32 and
/// 48 KiB the two were even, and from 64 KiB searching was 12-270% slower.
/// Below 2 KiB the levels split (-7 faster, -1 up to 48% slower on i686), and
/// every positive level searched slower at nearly every size, so both keep the
/// classifier.
///
/// All of that is on tables the frame finds warm. A search over noise touches
/// every page of its hash table, while the raw skip indexes a position in 512,
/// so on a workspace mapped fresh for the frame the search pays a fault per
/// table page: on musl, where a 10 KiB frame's workspace is past the mmap
/// threshold, searching made levels -7 and -1 63% slower, 73 us to 120 us.
///
/// Those measurements were on glibc, and musl does not follow them on warm
/// tables either: with a 10 KiB frame's workspace below musl's mmap threshold,
/// searching made levels -7 and -1 11-15% slower than asking the classifier (78
/// to 81 us against 89 to 91), so musl keeps the classifier at every size.
const SEARCH_INSTEAD_OF_CLASSIFIER: core::ops::RangeInclusive<usize> = 2 * 1024..=24 * 1024;

/// Whether a block is worth the classifier's fixed cost, given whether the
/// frame stores its literals raw and whether its tables sit on pages mapped
/// fresh for it. See [`SEARCH_INSTEAD_OF_CLASSIFIER`].
#[inline]
pub(crate) fn raw_skip_worth_asking(
    literals_stored_raw: bool,
    tables_on_fresh_pages: bool,
    block_len: usize,
) -> bool {
    !(!cfg!(target_env = "musl")
        && literals_stored_raw
        && !tables_on_fresh_pages
        && SEARCH_INSTEAD_OF_CLASSIFIER.contains(&block_len))
}

pub(crate) const RAW_FAST_PATH_MIN_BLOCK_LEN: usize = 512;
pub(crate) const RAW_FAST_PATH_MAX_SAMPLE_LEN: usize = 4096;
pub(crate) const RAW_FAST_PATH_MIN_SAMPLE_LEN: usize = 32;
/// How densely a block written off unsearched is still indexed.
///
/// It has to be findable at all, or a later block duplicating it has nothing
/// to match against; it does not have to be findable at every position. The
/// duplicate is recognised on the [`SeenContentGrid`] grid and then searched,
/// and the search sweeps positions, so an entry every `STEP` bytes is hit
/// within `STEP` bytes of scanning — immaterial against a block-sized match.
/// Indexing more finely is what the skip exists to avoid: at one entry per
/// eight bytes a megabyte of incompressible input costs 131,000 stores it had
/// no use for, which measured as a four-fold slowdown on the fast levels.
pub(crate) const RAW_SKIP_INDEX_STEP: usize = 512;

/// Window-size ceiling (8 MiB) above which a numeric level does not take the
/// skip whatever its number: the further back a match may reach, the more a
/// block written off unsearched can be throwing away. The three named levels
/// below `Best` resolve inside the band and always may.
const RAW_FAST_PATH_MAX_WINDOW_LOG: u8 = 23;
const RAW_FAST_PATH_MAX_WINDOW_SIZE_BYTES: u64 = 1u64 << RAW_FAST_PATH_MAX_WINDOW_LOG;

// Keep classifier scratch modest for no_std/small-stack targets: 1024 slots
// cuts per-call stack for repeat tracking from ~8 KiB to ~4 KiB.
const INCOMPRESSIBLE_REPEAT_TABLE_BITS: usize = 10;
const INCOMPRESSIBLE_REPEAT_TABLE_LEN: usize = 1 << INCOMPRESSIBLE_REPEAT_TABLE_BITS;
// 32-bit words: a 64-bit shift is two instructions and a branch on a 32-bit
// target.
const INCOMPRESSIBLE_REPEAT_OCCUPANCY_WORDS: usize = INCOMPRESSIBLE_REPEAT_TABLE_LEN / 32;
const INCOMPRESSIBLE_REPEAT_HASH_MULT: u32 = 0x9E37_79B1;

/// Top `INCOMPRESSIBLE_REPEAT_TABLE_BITS` bits of the 32-bit hash give the slot
/// directly (upstream zstd `ZSTD_hashPtr` shape).
#[inline(always)]
const fn repeat_slot(quad: u32) -> usize {
    (quad.wrapping_mul(INCOMPRESSIBLE_REPEAT_HASH_MULT) >> (32 - INCOMPRESSIBLE_REPEAT_TABLE_BITS))
        as usize
}

// The empty table holds 0 everywhere but in the slot 0 hashes to, which holds
// 1. A slot then never starts out holding a quad that hashes to it (0 lands in
// the one slot holding 1, and 1 lands elsewhere), so a fresh slot never reads as
// a repeat and no occupancy has to be tracked.
const _: () = assert!(repeat_slot(1) != repeat_slot(0));

#[inline(always)]
fn empty_repeat_table() -> [u32; INCOMPRESSIBLE_REPEAT_TABLE_LEN] {
    let mut table = [0u32; INCOMPRESSIBLE_REPEAT_TABLE_LEN];
    table[repeat_slot(0)] = 1;
    table
}
const INCOMPRESSIBLE_MIN_DISTINCT_BYTES: usize = 200;
// Allow at most ~4.2% concentration for the most frequent symbol in sampled data.
// This guards against low-entropy text-like inputs being misclassified as random.
const INCOMPRESSIBLE_MAX_SYMBOL_DIVISOR: usize = 24;
// Allow limited 4-byte hash-bucket repeats before treating the sample as structured.
const INCOMPRESSIBLE_REPEAT_DIVISOR: usize = 64;
/// Shortest sample counted into four byte tables rather than one (upstream
/// zstd `HIST_countFast_wksp`'s threshold).
const PARALLEL_HISTOGRAM_MIN_LEN: usize = 1500;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StrictProbeSelection {
    probe_len: usize,
    tail_start: Option<usize>,
    mid_start: Option<usize>,
}

impl StrictProbeSelection {
    #[inline]
    const fn reuses_full_block_classification(self) -> bool {
        self.tail_start.is_none()
    }
}

#[inline]
fn select_strict_probes(block_len: usize) -> StrictProbeSelection {
    let probe_len = RAW_FAST_PATH_MIN_BLOCK_LEN.min(block_len);
    if probe_len == block_len {
        StrictProbeSelection {
            probe_len,
            tail_start: None,
            mid_start: None,
        }
    } else {
        let tail_start = block_len - probe_len;
        if tail_start < probe_len {
            // For [probe_len + 1, 2 * probe_len), head/tail would heavily overlap.
            // Reuse the full-block classification computed by the caller.
            StrictProbeSelection {
                probe_len,
                tail_start: None,
                mid_start: None,
            }
        } else if tail_start < 2 * probe_len {
            // For [2 * probe_len, 3 * probe_len), head/tail are separable but a
            // distinct non-overlapping middle probe is not.
            StrictProbeSelection {
                probe_len,
                tail_start: Some(tail_start),
                mid_start: None,
            }
        } else {
            // Once we can separate all windows, use head/mid/tail probing.
            StrictProbeSelection {
                probe_len,
                tail_start: Some(tail_start),
                mid_start: Some(tail_start / 2),
            }
        }
    }
}

#[inline]
pub(crate) fn compression_level_allows_raw_fast_path(
    level: CompressionLevel,
    window_size: u64,
) -> bool {
    match level {
        // The window is the whole question, at every level. A level ceiling was
        // tried here — the high levels are where a block written off unsearched
        // costs the most — and it cost 12 to 32 times the encode on
        // incompressible input at levels 16 and up while buying nothing: what
        // the ceiling was guarding against is a repeat written off, and the
        // grid catches those on its own. Four megabytes repeated at nearly the
        // window distance comes out at 4,129,240 bytes against the reference's
        // 4,129,258 with the skip live at level 17.
        // Every level that compresses reads the same ceiling, the named ones
        // included: their preset window is well under it, but a public
        // `window_log` override moves the window without moving the level, and
        // a named level is then asking for a reach the grid's table cannot hold
        // records for while a numeric level asking for the same reach is
        // refused.
        CompressionLevel::Fastest
        | CompressionLevel::Default
        | CompressionLevel::Better
        | CompressionLevel::Best
        | CompressionLevel::Level(_) => window_size <= RAW_FAST_PATH_MAX_WINDOW_SIZE_BYTES,
        CompressionLevel::Uncompressed => false,
    }
}

/// Count the 4-byte quads of one sample region that repeat an earlier quad of
/// the same slot, across every region scanned with the same table.
///
/// Returns `true` as soon as the running count passes `repeat_guard`. That
/// guard is the FINAL threshold (fixed before any region is scanned) and the
/// count only grows, so an early `true` is exactly the verdict (compressible)
/// that a full scan would have produced. On repetitive data (structured text,
/// a single long match) the guard passes within the first few hundred bytes,
/// which is why this pass runs before the byte histogram: such a block never
/// pays for the histogram at all.
///
/// A loop of its own rather than one fused with the byte counts: fused, the
/// body carried four counter updates, the hash, the occupancy test and two
/// branches per quad, and its speed swung by more than a tenth with where the
/// linker happened to place it. Two narrow loops leave each body small.
#[inline]
fn count_quad_repeats(
    sample: &[u8],
    // Starts as the empty table described at `repeat_slot`.
    repeat_table: &mut [u32; INCOMPRESSIBLE_REPEAT_TABLE_LEN],
    repeats: &mut usize,
    repeat_guard: usize,
) -> bool {
    debug_assert!(sample.len() <= RAW_FAST_PATH_MAX_SAMPLE_LEN);
    // Whether `quad` repeats the one held in its slot, recording it either
    // way: on a hit the slot already holds `quad`, so the store changes
    // nothing and the body needs no branch.
    let mut seen = |quad: u32| {
        let slot = repeat_slot(quad);
        let hit = repeat_table[slot] == quad;
        repeat_table[slot] = quad;
        hit
    };
    // Two quads per iteration, in order: the second's slot may be the one the
    // first just wrote, as in a one-at-a-time walk.
    let (pairs, rest) = sample.as_chunks::<8>();
    for &pair in pairs {
        let word = u64::from_le_bytes(pair);
        *repeats += usize::from(seen(word as u32));
        *repeats += usize::from(seen((word >> 32) as u32));
        if *repeats > repeat_guard {
            return true;
        }
    }
    if let Some(&quad) = rest.as_chunks::<4>().0.first() {
        *repeats += usize::from(seen(u32::from_le_bytes(quad)));
    }
    *repeats > repeat_guard
}

/// The whole verdict for a sample under [`PARALLEL_HISTOGRAM_MIN_LEN`], in one
/// pass that counts bytes and quad repeats together.
///
/// A short sample cannot repay the long path's fixed work: clearing four
/// kilobytes of repeat table costs more than the occupancy bitset it replaces
/// saves over a few hundred quads. So the repeat table here is left uncleared
/// and read only behind its occupancy bit, and the counts are sixteen bits,
/// which a sample this short cannot overflow and which halve the clear.
fn short_sample_looks_incompressible(
    sample: &[u8],
    max_symbol_guard: usize,
    repeat_guard: usize,
) -> bool {
    debug_assert!(sample.len() < PARALLEL_HISTOGRAM_MIN_LEN);
    let mut counts = [0u16; 256];
    let mut repeat_table =
        [const { MaybeUninit::<u32>::uninit() }; INCOMPRESSIBLE_REPEAT_TABLE_LEN];
    let mut repeat_occupied = [0_u32; INCOMPRESSIBLE_REPEAT_OCCUPANCY_WORDS];
    // Repeats still allowed before the sample is called compressible: one value
    // where a count and its guard would be two. On i686 the loop has no register
    // to spare, and the second value pushed the data pointer to the stack, a
    // store and a reload on every quad.
    let mut repeats_left = repeat_guard + 1;
    let (quads, tail) = sample.as_chunks::<4>();
    for &chunk in quads {
        let quad = u32::from_le_bytes(chunk);
        counts[(quad & 0xFF) as usize] += 1;
        counts[((quad >> 8) & 0xFF) as usize] += 1;
        counts[((quad >> 16) & 0xFF) as usize] += 1;
        counts[(quad >> 24) as usize] += 1;
        let slot = repeat_slot(quad);
        let word = slot / 32;
        let bit = 1_u32 << (slot % 32);
        // SAFETY: the occupancy bit is set only in the arm below, right after
        // that slot is written, so a set bit means an initialised slot.
        if repeat_occupied[word] & bit != 0 && unsafe { repeat_table[slot].assume_init() } == quad {
            repeats_left -= 1;
            if repeats_left == 0 {
                return false;
            }
        } else {
            repeat_table[slot] = MaybeUninit::new(quad);
            repeat_occupied[word] |= bit;
        }
    }
    for &byte in tail {
        counts[usize::from(byte)] += 1;
    }
    let distinct = counts.iter().filter(|&&count| count != 0).count();
    let max_freq = counts.iter().copied().max().unwrap_or(0) as usize;
    distinct >= INCOMPRESSIBLE_MIN_DISTINCT_BYTES && max_freq <= max_symbol_guard
}

/// Add every byte of `sample` to `tables`, four tables summed by the caller
/// (upstream zstd `HIST_count_parallel_wksp`): consecutive bytes land in
/// different tables, so a run of one byte does not chain every increment on
/// the one before it through the same counter.
#[inline]
fn count_bytes(sample: &[u8], tables: &mut [[u32; 256]; 4]) {
    let (stripes, tail) = sample.as_chunks::<16>();
    for &stripe in stripes {
        let lo = u64::from_le_bytes(stripe[..8].try_into().expect("eight bytes"));
        let hi = u64::from_le_bytes(stripe[8..].try_into().expect("eight bytes"));
        for word in [lo, hi] {
            for half in [word as u32, (word >> 32) as u32] {
                tables[0][(half & 0xFF) as usize] += 1;
                tables[1][((half >> 8) & 0xFF) as usize] += 1;
                tables[2][((half >> 16) & 0xFF) as usize] += 1;
                tables[3][(half >> 24) as usize] += 1;
            }
        }
    }
    for &byte in tail {
        tables[0][usize::from(byte)] += 1;
    }
}

#[inline]
pub(crate) fn block_looks_incompressible(block: &[u8]) -> bool {
    if block.len() < RAW_FAST_PATH_MIN_BLOCK_LEN {
        return false;
    }
    sample_looks_incompressible(block)
}

#[inline]
pub(crate) fn block_looks_incompressible_strict(block: &[u8]) -> bool {
    if block.len() < RAW_FAST_PATH_MIN_BLOCK_LEN {
        return false;
    }
    if !sample_looks_incompressible(block) {
        return false;
    }
    // Best level should only early-exit on strongly random data. Probe head,
    // middle, and tail so mixed-entropy blocks do not get misclassified.
    let selection = select_strict_probes(block.len());
    if selection.reuses_full_block_classification() {
        // The full-block sample above already classified this input. For
        // minimum and near-min blocks, split probes would overlap too heavily.
        return true;
    }
    let probe_len = selection.probe_len;
    let tail_start = selection
        .tail_start
        .expect("strict probe tail_start should be present for split probes");
    let head = &block[..probe_len];
    let tail = &block[tail_start..tail_start + probe_len];
    if let Some(mid_start) = selection.mid_start {
        let mid = &block[mid_start..mid_start + probe_len];
        sample_looks_incompressible(head)
            && sample_looks_incompressible(mid)
            && sample_looks_incompressible(tail)
    } else {
        sample_looks_incompressible(head) && sample_looks_incompressible(tail)
    }
}

/// Whether a sample of at most [`RAW_FAST_PATH_MAX_SAMPLE_LEN`] bytes, the whole
/// block or its head, middle and tail, looks like noise.
fn sample_looks_incompressible(block: &[u8]) -> bool {
    let sample_len = block.len().min(RAW_FAST_PATH_MAX_SAMPLE_LEN);
    if sample_len < RAW_FAST_PATH_MIN_SAMPLE_LEN {
        return false;
    }

    // Select the sampled regions: the whole block when it fits the cap, or
    // head / middle / tail probes so capped samples still reject
    // mixed-entropy blocks whose center is compressible.
    let mut regions: [&[u8]; 3] = [&[], &[], &[]];
    let region_count = if sample_len == block.len() {
        regions[0] = block;
        1
    } else {
        let head_len = sample_len / 3;
        let mid_len = sample_len / 3;
        let tail_len = sample_len - head_len - mid_len;
        let mid_start = (block.len() - mid_len) / 2;
        regions[0] = &block[..head_len];
        regions[1] = &block[mid_start..mid_start + mid_len];
        regions[2] = &block[block.len() - tail_len..];
        3
    };

    // `repeat_guard` is the FINAL verdict threshold, fixed before scanning.
    // It needs the total 4-byte-quad count up front (one quad per 4 bytes of
    // each region) so `count_quad_repeats` can bail the moment the running
    // repeat count passes it.
    let max_symbol_guard = sample_len / INCOMPRESSIBLE_MAX_SYMBOL_DIVISOR;
    let total_quads: usize = regions[..region_count].iter().map(|r| r.len() / 4).sum();
    let repeat_guard = total_quads / INCOMPRESSIBLE_REPEAT_DIVISOR + 1;

    // Below this the separate passes cost more in fixed work (clearing the
    // repeat table and three extra byte tables, folding them) than they save,
    // as upstream zstd's `HIST_countFast_wksp` (hist.c:154) switches to a single
    // table at the same size.
    if sample_len < PARALLEL_HISTOGRAM_MIN_LEN {
        debug_assert_eq!(region_count, 1, "a sample under the cap is the whole block");
        return short_sample_looks_incompressible(block, max_symbol_guard, repeat_guard);
    }
    long_sample_looks_incompressible(&regions[..region_count], max_symbol_guard, repeat_guard)
}

/// The verdict for a sample of [`PARALLEL_HISTOGRAM_MIN_LEN`] bytes or more: the
/// quad repeats first, then the byte counts in four tables.
///
/// Out of line so that its eight kilobytes of tables are a frame of their own:
/// inlined, the caller carries that frame, and its stack probes, on every
/// short sample too, which never touches them.
///
/// The tables stay on the stack rather than in caller-owned scratch: both must
/// be emptied for every block anyway, so scratch would save only the stack
/// probes, and measured with the tables outside the frame that was flat on
/// x86_64 and 5-6% slower on i686.
#[inline(never)]
fn long_sample_looks_incompressible(
    regions: &[&[u8]],
    max_symbol_guard: usize,
    repeat_guard: usize,
) -> bool {
    let mut repeat_table = empty_repeat_table();
    let mut repeats = 0usize;
    for region in regions {
        if count_quad_repeats(region, &mut repeat_table, &mut repeats, repeat_guard) {
            // The repeat guard was passed — the block is compressible. This
            // is exactly the verdict a full scan would have produced.
            return false;
        }
    }

    let mut tables = [[0u32; 256]; 4];
    for region in regions {
        count_bytes(region, &mut tables);
    }
    let [total, second, third, fourth] = &mut tables;
    for (((a, b), c), d) in total.iter_mut().zip(&*second).zip(&*third).zip(&*fourth) {
        *a += b + c + d;
    }
    let mut distinct = 0usize;
    let mut max_freq = 0u32;
    for &count in total.iter() {
        distinct += usize::from(count != 0);
        max_freq = max_freq.max(count);
    }
    distinct >= INCOMPRESSIBLE_MIN_DISTINCT_BYTES && max_freq as usize <= max_symbol_guard
}

#[cfg(test)]
mod tests;
