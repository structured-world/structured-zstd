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
//! literals in bounds, a match from 16 or more bytes back inside the output
//! written so far, shorter than 256 bytes, the write and its overshoot inside
//! the output's room. Anything else leaves the loop with the sequence decoded
//! and the states advanced ([`Exit::Pending`]), and the caller executes it on
//! the Rust path, which handles every case. Near the start of the bit stream,
//! where a refill could reach before it, the loop leaves before decoding
//! ([`Exit::Boundary`]) and the caller runs that sequence whole.

use crate::bit_io::BitReaderReversed;
use crate::cpu_kernel::Avx2Kernel;
use crate::fse::{SeqFSEDecoder, SeqSymbol};

/// One FSE state advance: the entry in `$e` reads its `num_bits` (bits 16-23)
/// from the container and the next entry is `$slot`'s table at `new_state`
/// (bits 0-15, `$ew`) plus those bits.
macro_rules! seq_state_update {
    ($e:literal, $ew:literal, $slot:literal) => {
        concat!(
            "mov rax, ",
            $e,
            "\n",
            "shr eax, 16\n",
            "movzx edx, al\n",
            "add r9, rdx\n",
            "mov eax, 64\n",
            "sub rax, r9\n",
            "shrx rax, r8, rax\n",
            "bzhi rax, rax, rdx\n",
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

/// Lowest input position, in bytes from the stream's start, at which a pass
/// may begin: a pass consumes at most 31 + 16 + 16 + 9 + 9 + 8 bits over two
/// refills, which step back at most seven bytes each, so the second refill
/// still starts from eight bytes or more, where the Rust reader refills the
/// same way.
const INPUT_FLOOR: usize = 16;

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

/// Where the block's output stands, for the loop to read and advance.
pub(super) struct Out {
    /// Start of the linear output.
    pub(super) base: *mut u8,
    /// Write position, from `base`.
    pub(super) op: usize,
    /// The cursor's `cap_w`: a sequence's write must end at or before it.
    pub(super) cap_w: usize,
    /// The cursor's `live_base`: the live output is `op - live_base`, modular.
    pub(super) live_base: usize,
}

/// Runs up to `*left` sequences, each advancing the states, decrementing
/// `*left` for every one it executes.
///
/// # Safety
/// The CPU has BMI2 and AVX2. The reader was refilled after its last read and
/// its window `source[index..index + 8]` is in bounds; the three decoders hold
/// validated states of tables with `1 << accuracy_log` built entries. `out`
/// describes a linear output writable through `cap_w + 31`, whose bytes from
/// `live_base` to `op` are written. `literals` is readable `lit_len + 32` bytes
/// from its start. `*left >= 1`.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
pub(super) unsafe fn run(
    br: &mut BitReaderReversed<'_, Avx2Kernel>,
    ll_dec: &mut SeqFSEDecoder<'_>,
    ml_dec: &mut SeqFSEDecoder<'_>,
    of_dec: &mut SeqFSEDecoder<'_>,
    hist: &mut [u32; 3],
    out: &mut Out,
    literals: *const u8,
    lit_cur: &mut usize,
    lit_len: usize,
    left: &mut usize,
) -> Exit {
    debug_assert!(*left >= 1);
    debug_assert!(br.bits_consumed < 8);
    let source = br.source.as_ptr();
    let base = out.base as usize;
    let mut slots = Slots {
        hist: hist.map(u64::from),
        tables: [
            ll_dec.entry_words(),
            ml_dec.entry_words(),
            of_dec.entry_words(),
        ],
        input_floor: (source as usize).wrapping_add(INPUT_FLOOR),
        lit_end: literals as usize + lit_len,
        out_end: base + out.cap_w,
        out_floor: base.wrapping_add(out.live_base),
        left: *left,
        ll: 0,
        ml: 0,
        offset: 0,
    };
    let mut container = br.bit_container;
    let mut consumed = u64::from(br.bits_consumed);
    let mut ip = source.wrapping_add(br.index);
    let mut ll_e = ll_dec.state.to_word();
    let mut ml_e = ml_dec.state.to_word();
    let mut of_e = of_dec.state.to_word();
    let mut op = base + out.op;
    let mut lit = literals as usize + *lit_cur;
    let status: u64;
    // SAFETY: the caller's contract. The assembly reads the tables, the input
    // window, the literals and the output it has written, writes the output
    // within `out_end + 31` and the slots, and reads no input below the
    // floor's eight-byte window. Every field read is a plain integer.
    unsafe {
        core::arch::asm!(
            "2:",
            "cmp r10, qword ptr [rsi + 48]",
            "jb 90f",
            // Offset: a new one (more than one extra bit), or a repeat.
            "mov rax, r13",
            "shr rax, 24",
            "movzx edx, al",
            "cmp edx, 1",
            "jbe 50f",
            "shlx rax, r8, r9",
            "add r9, rdx",
            "neg edx",
            "shrx rax, rax, rdx",
            "mov rcx, r13",
            "shr rcx, 32",
            "lea rcx, [rcx + rax - 3]",
            "mov rax, qword ptr [rsi + 8]",
            "mov qword ptr [rsi + 16], rax",
            "mov rax, qword ptr [rsi]",
            "mov qword ptr [rsi + 8], rax",
            "mov qword ptr [rsi], rcx",
            // Match length.
            "20:",
            "mov rax, r12",
            "shr rax, 24",
            "movzx edi, al",
            "add r9, rdi",
            "mov eax, 64",
            "sub rax, r9",
            "shrx rax, r8, rax",
            "bzhi rdi, rax, rdi",
            "mov rax, r12",
            "shr rax, 32",
            "add rdi, rax",
            // Literal length, after a refill when its bits and the three state
            // updates would run past the container.
            "mov rax, r11",
            "shr rax, 24",
            "movzx edx, al",
            "lea rax, [r9 + rdx + 26]",
            "cmp rax, 64",
            "jbe 21f",
            "mov rax, r9",
            "shr rax, 3",
            "sub r10, rax",
            "and r9d, 7",
            "mov r8, qword ptr [r10]",
            "21:",
            "add r9, rdx",
            "mov eax, 64",
            "sub rax, r9",
            "shrx rax, r8, rax",
            "bzhi rdx, rax, rdx",
            "mov rax, r11",
            "shr rax, 32",
            "add rdx, rax",
            "mov qword ptr [rsi + 88], rdx",
            // The three states, LL, ML, OF.
            seq_state_update!("r11", "r11w", "24"),
            seq_state_update!("r12", "r12w", "32"),
            seq_state_update!("r13", "r13w", "40"),
            "mov rax, r9",
            "shr rax, 3",
            "sub r10, rax",
            "and r9d, 7",
            "mov r8, qword ptr [r10]",
            // The gates of the copies below; any refusal leaves the loop.
            "mov rdx, qword ptr [rsi + 88]",
            "lea rax, [r15 + rdx]",
            "cmp rax, qword ptr [rsi + 56]",
            "ja 80f",
            "cmp rdi, 255",
            "ja 80f",
            "cmp rcx, 16",
            "jb 80f",
            "lea rax, [r14 + rdx]",
            "add rax, rdi",
            "cmp rax, qword ptr [rsi + 64]",
            "ja 80f",
            "lea rax, [r14 + rdx]",
            "sub rax, qword ptr [rsi + 72]",
            "cmp rcx, rax",
            "ja 80f",
            // Literals: sixteen, then the rest sixteen at a time.
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
            "mov rcx, r11",
            "shr rcx, 32",
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
            "mov rdi, r11",
            "shr rdi, 32",
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
            inout("r11") ll_e,
            inout("r12") ml_e,
            inout("r13") of_e,
            inout("r14") op,
            inout("r15") lit,
            in("rsi") &raw mut slots,
            out("rax") status,
            out("rcx") _,
            out("rdx") _,
            out("rdi") _,
            out("xmm0") _,
            options(nostack),
        );
    }
    br.bit_container = container;
    br.bits_consumed = consumed as u8;
    br.index = ip as usize - source as usize;
    ll_dec.state = SeqSymbol::from_word(ll_e);
    ml_dec.state = SeqSymbol::from_word(ml_e);
    of_dec.state = SeqSymbol::from_word(of_e);
    // Every offset is below 2^32: a new one is `base + bits - 3` with
    // `bits < base <= 2^31`, a repeat is one of these or one less, modular.
    *hist = slots.hist.map(|h| h as u32);
    out.op = op - base;
    *lit_cur = lit - literals as usize;
    *left = slots.left;
    match status {
        0 => Exit::Done,
        1 => Exit::Boundary,
        _ => Exit::Pending {
            ll: slots.ll as u32,
            ml: slots.ml as u32,
            offset: slots.offset as u32,
        },
    }
}

#[cfg(test)]
mod tests;
