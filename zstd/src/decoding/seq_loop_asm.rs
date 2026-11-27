//! The common path of the AVX2 short sequence loop in assembly.
//!
//! Each pass decodes one sequence, advances the three FSE states, refills the
//! bit reader and executes the sequence, in the order and shape of upstream
//! zstd's `ZSTD_decompressSequences_body` (`ZSTD_decodeSequence` then
//! `ZSTD_execSequence`), in registers the compiler would not give the Rust
//! loop: the whole Rust loop is one function the allocator lays out as a unit,
//! and every edit to it so far has moved values to the stack rather than off it.
//!
//! The assembly executes only what it can do without asking the buffer: the
//! literals in bounds, a match inside the output written so far (from fewer
//! than 16 bytes back only with 16 bytes of output before it) or wholly inside
//! the dictionary with sixteen-byte reads to spare, shorter than 256 bytes,
//! the write and its overshoot inside the output's room. Anything else leaves
//! the loop with the sequence decoded and the states advanced
//! ([`Exit::Pending`]), and the caller executes it on the Rust path, which
//! handles every case. Near the start of the bit stream the window stops at
//! its first byte, as upstream's `BIT_reloadDStream` does; a corrupt stream
//! that consumes past the window leaves before decoding ([`Exit::Boundary`]).

use crate::bit_io::BitReaderReversed;
use crate::cpu_kernel::Avx2Kernel;
use crate::fse::{SeqFSEDecoder, SeqSymbol};

/// One FSE state advance: the entry in `$e` reads its `num_bits` (byte 2)
/// from the container and `$e` moves to `$slot`'s table at `new_state`
/// (bytes 0-1) plus those bits.
///
/// The loop holds its three entries in one of two forms (see [`Loop`]):
/// `pointers`, where `$e` points at the entry and every field loads straight
/// from it, as upstream zstd reads `ZSTD_seqSymbol`, and a state advance ends
/// in a `lea` into the table; or `words`, where `$e` holds the entry itself,
/// its fields are taken out by shifts, and an advance ends in a load of the
/// next entry (`$ew` is the low 16 bits of `$e`).
///
/// Every read of `n` bits at position `r9` here is
/// `((container << r9) >> 1) >> (63 ^ n)` (upstream zstd's
/// `BIT_lookBitsFast` shape, 0 for `n == 0`): the first two shifts need only
/// the old position, so after `n` is known only the `xor` and the last shift
/// stand between it and the bits, two fewer than `64 - (r9 + n)` with a
/// `bzhi`. `r9 + n <= 64` at every read, so `r9 == 64` only with `n == 0`.
macro_rules! seq_state_update {
    (pointers, $e:literal, $ew:literal, $slot:literal) => {
        concat!(
            "movzx edx, byte ptr [",
            $e,
            " + 2]\n",
            "shlx rax, r8, r9\n",
            "add r9, rdx\n",
            "shr rax, 1\n",
            "xor edx, 63\n",
            "shrx rax, rax, rdx\n",
            "movzx edx, word ptr [",
            $e,
            "]\n",
            "add rax, rdx\n",
            "mov rdx, qword ptr [rsi + ",
            $slot,
            "]\n",
            "lea ",
            $e,
            ", [rdx + rax*8]\n",
        )
    };
    (words, $e:literal, $ew:literal, $slot:literal) => {
        concat!(
            "mov rax, ",
            $e,
            "\n",
            "shr eax, 16\n",
            "movzx edx, al\n",
            "shlx rax, r8, r9\n",
            "add r9, rdx\n",
            "shr rax, 1\n",
            "xor edx, 63\n",
            "shrx rax, rax, rdx\n",
            "movzx edx, ",
            $ew,
            "\n",
            "add rax, rdx\n",
            "mov rdx, qword ptr [rsi + ",
            $slot,
            "]\n",
            "mov ",
            $e,
            ", qword ptr [rdx + rax*8]\n",
        )
    };
}
/// `$dst` takes the extra-bit count (byte 3) of the entry in `$e` (through
/// `rax` in the `words` form).
macro_rules! entry_extra {
    (pointers, $dst:literal, $e:literal) => {
        concat!("movzx ", $dst, ", byte ptr [", $e, " + 3]")
    };
    (words, $dst:literal, $e:literal) => {
        concat!("mov rax, ", $e, "\nshr rax, 24\nmovzx ", $dst, ", al")
    };
}

/// `$dst` (a 64-bit register, `$dst32` its low half) takes the base value
/// (bytes 4-7) of the entry in `$e`.
macro_rules! entry_base {
    (pointers, $dst:literal, $dst32:literal, $e:literal) => {
        concat!("mov ", $dst32, ", dword ptr [", $e, " + 4]")
    };
    (words, $dst:literal, $dst32:literal, $e:literal) => {
        concat!("mov ", $dst, ", ", $e, "\nshr ", $dst, ", 32")
    };
}

/// Lowest input position, in bytes from the stream's start, at which a pass
/// may begin: a pass consumes at most 31 + 16 + 16 + 9 + 9 + 8 bits over two
/// refills, which step back at most seven bytes each, so the second refill
/// still starts from eight bytes or more, where the Rust reader refills the
/// same way.
const INPUT_FLOOR: usize = 16;

/// Fewest sequences a block must have for the loop to be entered. Entering
/// costs about 150 instructions (the call, the loop state, the hand-back), and
/// a pass saves some 15 a sequence against the Rust loop, which in any case
/// runs the last sequences of the block below the floor. A 4 KiB frame of 13
/// sequences against a dictionary decoded about 5% slower for entering; at 64
/// the sequences the loop runs pay for the entry several times over.
const MIN_SEQUENCES: usize = 16;

/// Whether entering the loop for `steps` sequences pays: enough of them, and
/// the reader above the floor, the assembly's own first test, so a block the
/// loop would leave at once never enters it.
#[inline(always)]
pub(super) fn pays_to_enter(br: &BitReaderReversed<'_, Avx2Kernel>, steps: usize) -> bool {
    steps >= MIN_SEQUENCES && br.index >= INPUT_FLOOR
}

/// What the assembly reads and writes besides its registers, addressed
/// through `rsi`. The offsets are the assembly's.
#[repr(C)]
struct Slots {
    /// Offset history, `hist[0]` the most recent (0, 8, 16).
    hist: [u64; 3],
    /// First entry of the LL, ML and OF tables (24, 32, 40).
    tables: [*const u64; 3],
    /// Address below which a pass does not start (48).
    input_floor: usize,
    /// Address one past the last literal (56).
    lit_end: usize,
    /// Address the write of a sequence may reach, its overshoot left out (64).
    out_end: usize,
    /// Address the live output is measured from: a match offset is resident
    /// when it does not exceed the distance from here to the match (72).
    out_floor: usize,
    /// Sequences left to run, each advancing the states (80).
    left: usize,
    /// The decoded sequence on a pending exit (88, 96, 104).
    ll: u64,
    ml: u64,
    offset: u64,
    /// A dictionary match's literals must end below this address for the
    /// window to still hold the dictionary (112).
    dict_limit: usize,
    /// Dictionary content length (120) and the address one past its end (128).
    dict_len: usize,
    dict_end: usize,
    /// The LL, ML and OF entries at the start of a pass (136, 144, 152): the
    /// assembly holds pointers to entries, and these are where they point
    /// until the first state advance moves them into the tables.
    entries: [u64; 3],
    /// The byte shuffles of a match from fewer than 16 bytes back, indexed by
    /// its offset: the one spreading the period over a vector (160) and the
    /// one rotating it for the next chunk (168).
    period: *const [u8; 16],
    rotate: *const [u8; 16],
}

/// Why the loop stopped.
pub(super) enum Exit {
    /// Every sequence asked for ran.
    Done,
    /// The next sequence starts too near the start of the bit stream; nothing
    /// of it is decoded.
    Boundary,
    /// This sequence is decoded, the offset history and the three states
    /// advanced and the reader refilled, but it is not executed.
    Pending { ll: u32, ml: u32, offset: u32 },
}

/// The block's output bounds, fixed for the block.
pub(super) struct Out {
    /// Start of the linear output.
    pub(super) base: *mut u8,
    /// The cursor's `cap_w`: a sequence's write must end at or before it.
    pub(super) cap_w: usize,
    /// The cursor's `live_base`: the live output is `op - live_base`, modular.
    pub(super) live_base: usize,
    /// The cursor's `dict_end`: a dictionary match's literals end below it.
    pub(super) dict_end: usize,
}

/// The loop's state for one block: the slots, and the bit reader, the three
/// FSE entries and the offset history taken out of their owners once, so a
/// pending exit hands back only the output and literal positions, the only
/// things the caller's execution of that sequence moves. [`Loop::finish`]
/// returns the rest to the owners.
///
/// `WORDS` is how the assembly holds its three FSE entries, both forms
/// decoding the same: as words, their fields taken out by shifts, or as
/// pointers into the tables with the fields loaded from memory. The pointers
/// shorten the chain from one sequence's entries to the next's and win on
/// most cores; the words win on Intel's Golden Cove line (see
/// [`crate::cpu_kernel::detect_seq_entries_as_words`]). Chosen once per block
/// above the loop, so a pass carries no test of it.
pub(super) struct Loop<const WORDS: bool> {
    slots: Slots,
    container: u64,
    consumed: u64,
    ip: *const u8,
    ll_e: u64,
    ml_e: u64,
    of_e: u64,
    /// The LL, ML and OF entries in their tables once a pass has advanced
    /// the states, null before: later passes start from them, so no pass
    /// but the first reads its entries back from a store just made.
    at: [*const u64; 3],
    source: *const u8,
    base: usize,
    literals: usize,
}

impl<const WORDS: bool> Loop<WORDS> {
    /// Takes the block's reader, states and history, to run `left`
    /// sequences, each advancing the states.
    ///
    /// # Safety
    /// The CPU has BMI2 and AVX2. The reader was refilled after its last read
    /// and its window `source[index..index + 8]` is in bounds; the three
    /// decoders hold validated states of tables with `1 << accuracy_log` built
    /// entries, and are left untouched until [`Loop::finish`]. `out` describes
    /// a linear output writable through `cap_w + 31`, and the window holds
    /// `dict_content` for a match whose literals end below `dict_end`.
    /// `literals` is readable `lit_len + 32` bytes from its start. The loop
    /// pays to enter ([`pays_to_enter`]), so the first pass decodes a
    /// sequence before it can stop.
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    pub(super) unsafe fn new(
        br: &BitReaderReversed<'_, Avx2Kernel>,
        ll_dec: &SeqFSEDecoder<'_>,
        ml_dec: &SeqFSEDecoder<'_>,
        of_dec: &SeqFSEDecoder<'_>,
        hist: &[u32; 3],
        out: &Out,
        literals: *const u8,
        lit_len: usize,
        dict_content: &[u8],
        left: usize,
    ) -> Self {
        debug_assert!(br.bits_consumed < 8);
        debug_assert!(pays_to_enter(br, left));
        let source = br.source.as_ptr();
        let base = out.base as usize;
        Self {
            slots: Slots {
                hist: hist.map(u64::from),
                tables: [
                    ll_dec.entry_words(),
                    ml_dec.entry_words(),
                    of_dec.entry_words(),
                ],
                input_floor: source as usize,
                lit_end: literals as usize + lit_len,
                out_end: base + out.cap_w,
                out_floor: base.wrapping_add(out.live_base),
                left,
                ll: 0,
                ml: 0,
                offset: 0,
                // Saturating is the meaning: a position past the address space
                // means the window never runs out within this output, which
                // the largest address says as well; zero (no dictionary room)
                // gives `base`, below every write position.
                dict_limit: base.saturating_add(out.dict_end),
                dict_len: dict_content.len(),
                dict_end: dict_content.as_ptr() as usize + dict_content.len(),
                entries: [0; 3],
                period: crate::decoding::exec_sequence_inline::x86::PERIOD.as_ptr(),
                rotate: crate::decoding::exec_sequence_inline::x86::ROTATE.as_ptr(),
            },
            container: br.bit_container,
            consumed: u64::from(br.bits_consumed),
            ip: source.wrapping_add(br.index),
            ll_e: ll_dec.state.to_word(),
            ml_e: ml_dec.state.to_word(),
            of_e: of_dec.state.to_word(),
            at: [core::ptr::null(); 3],
            source,
            base,
            literals: literals as usize,
        }
    }

    /// Sequences still to run.
    #[inline(always)]
    pub(super) fn left(&self) -> usize {
        self.slots.left
    }

    /// Counts a pending sequence the caller executed.
    #[inline(always)]
    pub(super) fn executed_pending(&mut self) {
        self.slots.left -= 1;
    }

    /// Returns the reader, the states and the history to their owners.
    #[inline(always)]
    pub(super) fn finish(
        self,
        br: &mut BitReaderReversed<'_, Avx2Kernel>,
        ll_dec: &mut SeqFSEDecoder<'_>,
        ml_dec: &mut SeqFSEDecoder<'_>,
        of_dec: &mut SeqFSEDecoder<'_>,
        hist: &mut [u32; 3],
    ) {
        br.bit_container = self.container;
        br.bits_consumed = self.consumed as u8;
        br.index = self.ip as usize - self.source as usize;
        let [ll, ml, of] = if self.at[0].is_null() {
            [self.ll_e, self.ml_e, self.of_e]
        } else {
            // SAFETY: entries of the three tables, readable eight-byte words;
            // `SeqSymbol` aligns to four bytes only, so the read is unaligned
            // (one `mov` on x86, as the loop's own loads).
            unsafe { self.at.map(|p| p.read_unaligned()) }
        };
        ll_dec.state = SeqSymbol::from_word(ll);
        ml_dec.state = SeqSymbol::from_word(ml);
        of_dec.state = SeqSymbol::from_word(of);
        // Every offset is below 2^32: a new one is `base + bits - 3` with
        // `bits < base <= 2^31`, a repeat is one of these or one less, modular.
        *hist = self.slots.hist.map(|h| h as u32);
    }

    /// Runs sequences until one leaves, or none is left (`left() >= 1` on
    /// entry), from output position `*op_pos` (from the output's start) and
    /// literal position `*lit_cur`, both advanced past what ran.
    ///
    /// # Safety
    /// [`Loop::new`]'s contract holds, the output's bytes up to `*op_pos` are
    /// written, and every pending exit's sequence was executed and counted.
    #[inline(always)]
    pub(super) unsafe fn pass(&mut self, op_pos: &mut usize, lit_cur: &mut usize) -> Exit {
        debug_assert!(self.slots.left >= 1);
        let mut container = self.container;
        let mut consumed = self.consumed;
        let mut ip = self.ip;
        let slots = &raw mut self.slots;
        // SAFETY: a field address inside `self.slots`, which outlives the
        // assembly.
        let entries = unsafe { (&raw mut (*slots).entries).cast::<u64>().cast_const() };
        let mut op = self.base + *op_pos;
        let mut lit = self.literals + *lit_cur;
        let status: u64;
        // The loop in the entry form `$form`, with the entries in `$ll_e`,
        // `$ml_e` and `$of_e` (pointers or words).
        macro_rules! seq_pass_asm {
            ($form:tt, $ll_e:ident, $ml_e:ident, $of_e:ident) => {
            core::arch::asm!(
                "2:",
                // More than the container consumed: only a corrupt stream
                // gets here; the Rust path reports it.
                "cmp r9, 64",
                "ja 90f",
                // Offset: a new one (more than one extra bit), or a repeat.
                entry_extra!($form, "edx", "r13"),
                "cmp edx, 1",
                "jbe 50f",
                "shlx rax, r8, r9",
                "add r9, rdx",
                "neg edx",
                "shrx rax, rax, rdx",
                entry_base!($form, "rcx", "ecx", "r13"),
                "lea rcx, [rcx + rax - 3]",
                "mov rax, qword ptr [rsi + 8]",
                "mov qword ptr [rsi + 16], rax",
                "mov rax, qword ptr [rsi]",
                "mov qword ptr [rsi + 8], rax",
                "mov qword ptr [rsi], rcx",
                // Match length.
                "20:",
                entry_extra!($form, "edi", "r12"),
                "shlx rax, r8, r9",
                "add r9, rdi",
                "shr rax, 1",
                "xor edi, 63",
                "shrx rdi, rax, rdi",
                entry_base!($form, "rax", "eax", "r12"),
                "add rdi, rax",
                // Literal length, after a refill when its bits and the three state
                // updates would run past the container.
                entry_extra!($form, "edx", "r11"),
                "lea rax, [r9 + rdx + 26]",
                "cmp rax, 64",
                "jbe 21f",
                "mov rax, r9",
                "shr rax, 3",
                "sub r10, rax",
                "and r9d, 7",
                // Near the stream's start the window stops at its first byte
                // and the bits it could not step back stay counted as
                // consumed, as upstream's `BIT_reloadDStream` does.
                "cmp r10, qword ptr [rsi + 48]",
                "jae 22f",
                "mov rax, qword ptr [rsi + 48]",
                "sub rax, r10",
                "lea r9, [r9 + rax*8]",
                "mov r10, qword ptr [rsi + 48]",
                "22:",
                "mov r8, qword ptr [r10]",
                "21:",
                "shlx rax, r8, r9",
                "add r9, rdx",
                "shr rax, 1",
                "xor edx, 63",
                "shrx rdx, rax, rdx",
                entry_base!($form, "rax", "eax", "r11"),
                "add rdx, rax",
                "mov qword ptr [rsi + 88], rdx",
                // The three states, LL, ML, OF.
                seq_state_update!($form, "r11", "r11w", "24"),
                seq_state_update!($form, "r12", "r12w", "32"),
                seq_state_update!($form, "r13", "r13w", "40"),
                "mov rax, r9",
                "shr rax, 3",
                "sub r10, rax",
                "and r9d, 7",
                "cmp r10, qword ptr [rsi + 48]",
                "jae 23f",
                "mov rax, qword ptr [rsi + 48]",
                "sub rax, r10",
                "lea r9, [r9 + rax*8]",
                "mov r10, qword ptr [rsi + 48]",
                "23:",
                "mov r8, qword ptr [r10]",
                // The gates of the copies below; any refusal leaves the loop.
                "mov rdx, qword ptr [rsi + 88]",
                "lea rax, [r15 + rdx]",
                "cmp rax, qword ptr [rsi + 56]",
                "ja 80f",
                "cmp rdi, 255",
                "ja 80f",
                "lea rax, [r14 + rdx]",
                "add rax, rdi",
                "cmp rax, qword ptr [rsi + 64]",
                "ja 80f",
                "lea rax, [r14 + rdx]",
                "sub rax, qword ptr [rsi + 72]",
                "cmp rcx, rax",
                "ja 60f",
                "cmp rcx, 16",
                "jb 70f",
                // Literals: sixteen, then the rest sixteen at a time.
                "33:",
                "vmovdqu xmm0, xmmword ptr [r15]",
                "vmovdqu xmmword ptr [r14], xmm0",
                "cmp rdx, 16",
                "ja 30f",
                "31:",
                "add r15, rdx",
                "add r14, rdx",
                // Match, from 16 or more bytes back: the same.
                "mov rax, r14",
                "sub rax, rcx",
                "vmovdqu xmm0, xmmword ptr [rax]",
                "vmovdqu xmmword ptr [r14], xmm0",
                "cmp rdi, 16",
                "ja 40f",
                "41:",
                "add r14, rdi",
                "dec qword ptr [rsi + 80]",
                "jnz 2b",
                "xor eax, eax",
                "jmp 99f",
                // A repeated offset with no extra bit.
                "50:",
                "je 52f",
                "mov rax, qword ptr [rsi]",
                "mov rdi, qword ptr [rsi + 8]",
                entry_base!($form, "rcx", "ecx", "r11"),
                "test ecx, ecx",
                "mov rcx, rax",
                "cmovz rcx, rdi",
                "cmovz rdi, rax",
                "mov qword ptr [rsi + 8], rdi",
                "mov qword ptr [rsi], rcx",
                "jmp 20b",
                // A repeated offset with one extra bit: index 1 + (ll == 0) + bit,
                // index 3 the most recent less one.
                "52:",
                "shlx rax, r8, r9",
                "shr rax, 63",
                "inc r9",
                entry_base!($form, "rdi", "edi", "r11"),
                "cmp edi, 1",
                "adc rax, 1",
                "cmp rax, 3",
                "jne 53f",
                "mov rcx, qword ptr [rsi]",
                "lea ecx, [rcx - 1]",
                "jmp 54f",
                "53:",
                "mov rcx, qword ptr [rsi + rax*8]",
                "54:",
                "cmp rax, 1",
                "je 55f",
                "mov rdi, qword ptr [rsi + 8]",
                "mov qword ptr [rsi + 16], rdi",
                "55:",
                "mov rdi, qword ptr [rsi]",
                "mov qword ptr [rsi + 8], rdi",
                "mov qword ptr [rsi], rcx",
                "jmp 20b",
                // The rest of a literal run past sixteen.
                "30:",
                "mov eax, 16",
                "32:",
                "vmovdqu xmm0, xmmword ptr [r15 + rax]",
                "vmovdqu xmmword ptr [r14 + rax], xmm0",
                "add rax, 16",
                "cmp rax, rdx",
                "jb 32b",
                "jmp 31b",
                // The rest of a match past sixteen; `rax` is its source.
                "40:",
                "mov edx, 16",
                "42:",
                "vmovdqu xmm0, xmmword ptr [rax + rdx]",
                "vmovdqu xmmword ptr [r14 + rdx], xmm0",
                "add rdx, 16",
                "cmp rdx, rdi",
                "jb 42b",
                "jmp 41b",
                // A match reaching past the output (`rax` the output behind it):
                // taken when its literals end below the dictionary limit and the
                // match lies inside the dictionary with its reads rounded up to
                // sixteen, as the Rust path's extDict branch takes it. `rcx`
                // becomes the distance from the match's write position to its
                // dictionary source, so the copies above run unchanged.
                "60:",
                "lea rdx, [r14 + rdx]",
                "cmp rdx, qword ptr [rsi + 112]",
                "jae 80f",
                "neg rax",
                "add rax, rcx",
                "cmp rax, qword ptr [rsi + 120]",
                "ja 80f",
                "lea rdx, [rdi + 15]",
                "and rdx, -16",
                "cmp rdx, rax",
                "ja 80f",
                "mov rdx, qword ptr [rsi + 128]",
                "sub rdx, rax",
                "mov rcx, qword ptr [rsi + 88]",
                "add rcx, r14",
                "sub rcx, rdx",
                "mov rdx, qword ptr [rsi + 88]",
                "jmp 33b",
                // A match from fewer than 16 bytes back repeats its own output
                // with period `rcx`: the literals as above, then one shuffle
                // spreads the period over a vector from the 16 bytes before the
                // match, and a second rotates it for each further chunk (the
                // Rust path's `overlap_match_shuffle`). `rax` is the live output
                // at the match, which must hold those 16 bytes.
                "70:",
                "cmp rax, 16",
                "jb 80f",
                // A zero offset (malformed input) is the Rust path's to refuse.
                "test rcx, rcx",
                "jz 80f",
                "vmovdqu xmm0, xmmword ptr [r15]",
                "vmovdqu xmmword ptr [r14], xmm0",
                "cmp rdx, 16",
                "jbe 72f",
                "mov eax, 16",
                "73:",
                "vmovdqu xmm0, xmmword ptr [r15 + rax]",
                "vmovdqu xmmword ptr [r14 + rax], xmm0",
                "add rax, 16",
                "cmp rax, rdx",
                "jb 73b",
                "72:",
                "add r15, rdx",
                "add r14, rdx",
                "mov rdx, rcx",
                "shl rdx, 4",
                "mov rax, qword ptr [rsi + 160]",
                "vmovdqu xmm0, xmmword ptr [r14 - 16]",
                "vpshufb xmm0, xmm0, xmmword ptr [rax + rdx]",
                "mov rax, qword ptr [rsi + 168]",
                "vmovdqu xmm1, xmmword ptr [rax + rdx]",
                "xor edx, edx",
                "74:",
                "vmovdqu xmmword ptr [r14 + rdx], xmm0",
                "add rdx, 16",
                "cmp rdx, rdi",
                "jae 41b",
                "vpshufb xmm0, xmm0, xmm1",
                "jmp 74b",
                "80:",
                "mov qword ptr [rsi + 96], rdi",
                "mov qword ptr [rsi + 104], rcx",
                "mov eax, 2",
                "jmp 99f",
                "90:",
                "mov eax, 1",
                "99:",
                inout("r8") container,
                inout("r9") consumed,
                inout("r10") ip,
                inout("r11") $ll_e,
                inout("r12") $ml_e,
                inout("r13") $of_e,
                inout("r14") op,
                inout("r15") lit,
                in("rsi") slots,
                out("rax") status,
                out("rcx") _,
                out("rdx") _,
                out("rdi") _,
                out("xmm0") _,
                out("xmm1") _,
                options(nostack),
            )
            };
        }
        // A constant of the monomorph: the other body is not compiled into it.
        match WORDS {
            false => {
                if self.at[0].is_null() {
                    self.slots.entries = [self.ll_e, self.ml_e, self.of_e];
                    self.at = [entries, entries.wrapping_add(1), entries.wrapping_add(2)];
                }
                let [mut ll_e, mut ml_e, mut of_e] = self.at;
                // SAFETY: the caller's contract. The assembly reads the
                // tables, the input window, the literals and the output it has
                // written, writes the output within `out_end + 31` and the
                // slots, and reads no input below the floor's eight-byte
                // window. Every field read is a plain integer.
                unsafe { seq_pass_asm!(pointers, ll_e, ml_e, of_e) };
                // The first pass decodes a sequence before it can stop (see
                // [`Loop::new`]) and the states advance together, so all
                // three pointers are in the tables now, none at the slots
                // (which would not follow a move of `self`).
                debug_assert_ne!(ll_e, entries);
                self.at = [ll_e, ml_e, of_e];
            }
            true => {
                let [mut ll_e, mut ml_e, mut of_e] = [self.ll_e, self.ml_e, self.of_e];
                // SAFETY: as for the pointer form.
                unsafe { seq_pass_asm!(words, ll_e, ml_e, of_e) };
                [self.ll_e, self.ml_e, self.of_e] = [ll_e, ml_e, of_e];
            }
        }
        self.container = container;
        self.consumed = consumed;
        self.ip = ip;
        *op_pos = op - self.base;
        *lit_cur = lit - self.literals;
        #[cfg(all(test, feature = "std"))]
        tests::count_exit(status as usize);
        match status {
            0 => Exit::Done,
            1 => Exit::Boundary,
            _ => Exit::Pending {
                ll: self.slots.ll as u32,
                ml: self.slots.ml as u32,
                offset: self.slots.offset as u32,
            },
        }
    }
}

#[cfg(all(test, feature = "std"))]
pub(super) mod tests;
