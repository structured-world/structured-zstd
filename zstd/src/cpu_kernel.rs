//! CPU kernel dispatch — single detect+match at the dispatch site,
//! propagated through the inner pipeline as a generic parameter so
//! leaf hot-path code monomorphises against the chosen kernel.
//!
//! See issue #247 for the architecture rationale: per-subsystem
//! dispatch scatters the choice across HUF / FSE / SIMD-copy
//! independently and pays the cost N times per call. Lifting the
//! dispatch to the outermost feasible call site collapses it to one
//! detect there; the inner leaf-hot-path ops then route through
//! `K::method` calls on the chosen kernel zero-sized type.
//!
//! Current wiring (as of #247 Part 2): the only active dispatch site
//! is `decoding::literals_section_decoder::decompress_literals`,
//! which `match`es `detect_cpu_kernel()` and routes into per-K
//! `decompress_literals_*` `#[target_feature]` wrappers. The full
//! pipeline-wide propagation envisioned in the issue (FrameDecoder /
//! FrameCompressor entry, sequence executor, match copy) is
//! incremental; subsequent tiers extend the dispatch surface without
//! changing this trait or the kernel ZSTs.
//!
//! Structure code (block loop, FCS check, offset history, repeat
//! semantics) stays single-impl and only carries `K` as a phantom on
//! the outer function. Monomorphisation specialises ONLY the bodies
//! that actually differ per ISA — `mask_lower_bits`, `huf_burst`,
//! `copy_chunk`, etc.

#[cfg(feature = "std")]
use std::sync::OnceLock;

/// An instruction-set level the codec's kernels may be limited to, from the
/// portable scalar code up. The x86 levels (`Sse2` through `Avx512`) and the
/// aarch64 ones (`Neon`, `Sve`) are separate ladders; `Scalar` sits under
/// both.
///
/// Used with [`set_cpu_ceiling`] to compare kernel tiers on one machine.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CpuLevel {
    /// Portable code only.
    Scalar,
    /// x86 SSE2.
    Sse2,
    /// x86 SSE4.2.
    Sse42,
    /// x86 BMI2, with everything below it.
    Bmi2,
    /// x86 AVX2 (and BMI2), the x86-64-v3 kernels.
    Avx2,
    /// x86 AVX-512 (the VBMI2 kernels, when built with `kernel-vbmi2`).
    Avx512,
    /// aarch64 NEON.
    Neon,
    /// aarch64 SVE and SVE2.
    Sve,
}

impl CpuLevel {
    /// Every level, lowest first within each ladder.
    pub const ALL: [CpuLevel; 8] = [
        CpuLevel::Scalar,
        CpuLevel::Sse2,
        CpuLevel::Sse42,
        CpuLevel::Bmi2,
        CpuLevel::Avx2,
        CpuLevel::Avx512,
        CpuLevel::Neon,
        CpuLevel::Sve,
    ];

    /// The level's name as [`FromStr`](core::str::FromStr) reads it.
    ///
    /// # Examples
    /// ```
    /// use structured_zstd::CpuLevel;
    ///
    /// assert_eq!(CpuLevel::Sse42.name(), "sse4.2");
    /// assert_eq!("avx2".parse::<CpuLevel>(), Ok(CpuLevel::Avx2));
    /// ```
    pub const fn name(self) -> &'static str {
        match self {
            CpuLevel::Scalar => "scalar",
            CpuLevel::Sse2 => "sse2",
            CpuLevel::Sse42 => "sse4.2",
            CpuLevel::Bmi2 => "bmi2",
            CpuLevel::Avx2 => "avx2",
            CpuLevel::Avx512 => "avx512",
            CpuLevel::Neon => "neon",
            CpuLevel::Sve => "sve",
        }
    }

    /// Which ladder the level is on, and its rung: 0 for `Scalar` on either.
    const fn ladder(self) -> (Ladder, u8) {
        match self {
            CpuLevel::Scalar => (Ladder::Any, 0),
            CpuLevel::Sse2 => (Ladder::X86, 1),
            CpuLevel::Sse42 => (Ladder::X86, 2),
            CpuLevel::Bmi2 => (Ladder::X86, 3),
            CpuLevel::Avx2 => (Ladder::X86, 4),
            CpuLevel::Avx512 => (Ladder::X86, 5),
            CpuLevel::Neon => (Ladder::Arm, 1),
            CpuLevel::Sve => (Ladder::Arm, 2),
        }
    }

    /// Whether the level exists on the architecture this build targets.
    const fn native(self) -> bool {
        match self.ladder().0 {
            Ladder::Any => true,
            Ladder::X86 => cfg!(any(target_arch = "x86", target_arch = "x86_64")),
            Ladder::Arm => cfg!(target_arch = "aarch64"),
        }
    }
}

impl core::fmt::Display for CpuLevel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

/// A name that is not a [`CpuLevel`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct UnknownCpuLevel;

impl core::fmt::Display for UnknownCpuLevel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("unknown CPU level; expected one of ")?;
        for (index, level) in CpuLevel::ALL.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            f.write_str(level.name())?;
        }
        Ok(())
    }
}

#[cfg(feature = "std")]
impl std::error::Error for UnknownCpuLevel {}

impl core::str::FromStr for CpuLevel {
    type Err = UnknownCpuLevel;

    /// The names [`CpuLevel::name`] gives, case-insensitive, plus the spellings
    /// `sse42`, `avx-512` and `avx512f`.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let lower = |expected: &str| text.eq_ignore_ascii_case(expected);
        CpuLevel::ALL
            .into_iter()
            .find(|level| lower(level.name()))
            .or_else(|| {
                if lower("sse42") {
                    Some(CpuLevel::Sse42)
                } else if lower("avx-512") || lower("avx512f") {
                    Some(CpuLevel::Avx512)
                } else {
                    None
                }
            })
            .ok_or(UnknownCpuLevel)
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Ladder {
    Any,
    X86,
    Arm,
}

/// Why [`set_cpu_ceiling`] could not set the ceiling.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CpuCeilingError {
    /// A kernel was already chosen, or a ceiling already set: the tiers in
    /// use cannot change for the rest of the process.
    AlreadyResolved,
    /// The level belongs to another architecture than this build's.
    OtherArchitecture(CpuLevel),
    /// This target has no atomics to hold a ceiling in.
    Unsupported,
}

impl core::fmt::Display for CpuCeilingError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CpuCeilingError::AlreadyResolved => {
                f.write_str("the CPU kernels were already chosen; set the ceiling before any work")
            }
            CpuCeilingError::OtherArchitecture(level) => {
                write!(f, "{level} is not an instruction set of this architecture")
            }
            CpuCeilingError::Unsupported => f.write_str("this target cannot hold a CPU ceiling"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for CpuCeilingError {}

/// The ceiling, frozen at its first read: 0 is not yet read or set,
/// [`CEILING_NONE`] is frozen without a limit, anything else is a level's
/// index in [`CpuLevel::ALL`] plus one.
#[cfg(target_has_atomic = "8")]
static CEILING: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
#[cfg(target_has_atomic = "8")]
const CEILING_NONE: u8 = u8::MAX;

/// Limit every kernel the codec picks at run time to `level` and below, for
/// the rest of the process. A kernel is still used only where the CPU
/// supports it: the ceiling never raises a choice.
///
/// Only the run-time choices are limited. A build compiled for a wider
/// baseline (`-C target-cpu=...`) uses those instructions throughout, below
/// any ceiling.
///
/// # Errors
///
/// [`CpuCeilingError::AlreadyResolved`] once any kernel has been chosen or a
/// ceiling set. Building a codec context can already choose kernels (a
/// decoder does, at construction), and so does querying one
/// ([`active_cpu_kernel_name`],
/// [`cpu_ceiling`]): set the ceiling before creating any context, not merely
/// before the first compression or decompression.
/// [`CpuCeilingError::OtherArchitecture`] for a level of another architecture.
///
/// # Examples
/// ```standalone_crate
/// use structured_zstd::{CpuLevel, set_cpu_ceiling};
///
/// // Before any compression or decompression in the process.
/// set_cpu_ceiling(CpuLevel::Scalar).unwrap();
/// assert_eq!(structured_zstd::cpu_ceiling(), Some(CpuLevel::Scalar));
/// ```
pub fn set_cpu_ceiling(level: CpuLevel) -> Result<(), CpuCeilingError> {
    if !level.native() {
        return Err(CpuCeilingError::OtherArchitecture(level));
    }
    #[cfg(target_has_atomic = "8")]
    {
        use core::sync::atomic::Ordering;
        let index = CpuLevel::ALL
            .iter()
            .position(|candidate| *candidate == level)
            .expect("every level is in ALL") as u8;
        CEILING
            .compare_exchange(0, index + 1, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| CpuCeilingError::AlreadyResolved)
    }
    #[cfg(not(target_has_atomic = "8"))]
    {
        Err(CpuCeilingError::Unsupported)
    }
}

/// The ceiling in force, freezing it (as none) if nothing set one yet.
pub fn cpu_ceiling() -> Option<CpuLevel> {
    #[cfg(target_has_atomic = "8")]
    {
        use core::sync::atomic::Ordering;
        // Once frozen the value never changes, so a plain load answers every
        // later call; only the first read pays the locked compare-exchange.
        let mut stored = CEILING.load(Ordering::Acquire);
        if stored == 0 {
            stored = match CEILING.compare_exchange(
                0,
                CEILING_NONE,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => CEILING_NONE,
                Err(stored) => stored,
            };
        }
        (stored != CEILING_NONE).then(|| CpuLevel::ALL[usize::from(stored - 1)])
    }
    #[cfg(not(target_has_atomic = "8"))]
    {
        None
    }
}

/// Whether a kernel needing `needed` may run under `ceiling`.
// Only a build that compiles a run-time kernel choice asks; wasm, or a target
// with its kernel features off, compiles none.
#[cfg_attr(
    not(any(
        test,
        target_arch = "x86_64",
        all(target_arch = "x86", feature = "kernel-sse"),
        all(target_arch = "aarch64", feature = "kernel-neon"),
    )),
    allow(dead_code)
)]
const fn allowed_under(ceiling: Option<CpuLevel>, needed: CpuLevel) -> bool {
    let Some(ceiling) = ceiling else {
        return true;
    };
    let (needed_ladder, needed_rung) = needed.ladder();
    let (ceiling_ladder, ceiling_rung) = ceiling.ladder();
    needed_rung == 0 || (needed_ladder as u8 == ceiling_ladder as u8 && needed_rung <= ceiling_rung)
}

/// Whether a kernel needing `needed` may be chosen. Every run-time kernel
/// selection asks this beside its CPU-feature probe.
#[cfg_attr(
    not(any(
        target_arch = "x86_64",
        all(target_arch = "x86", feature = "kernel-sse"),
        all(target_arch = "aarch64", feature = "kernel-neon"),
    )),
    allow(dead_code)
)]
#[inline]
pub(crate) fn cpu_allows(needed: CpuLevel) -> bool {
    allowed_under(cpu_ceiling(), needed)
}

/// Trait covering the leaf hot-path operations whose bodies differ
/// per ISA. Implementations are ZSTs; the trait is `Copy` so it can
/// be `Default`-constructed at each call site without runtime cost.
///
/// New methods land here ONLY when their codegen genuinely differs
/// per kernel (BMI2 intrinsic vs scalar shift, AVX2 256-bit move vs
/// SSE2 128-bit move, etc.). Structure ops that have one canonical
/// implementation must NOT be on this trait — they stay on the
/// existing decoder / encoder types.
// Public (rather than `pub(crate)`) because `BitReaderReversed` is
// generic over `K: CpuKernel = ScalarKernel` and is re-exported via
// the `bench-internals`-gated `testing` module; under that feature
// the visibility of every type that appears in `BitReaderReversed`'s
// bounds (the trait + the default kernel) must match the type's own
// visibility, otherwise rustc rejects with `private_bounds` /
// `private_interfaces`. The trait surface stays narrow on stable
// crate users: nothing outside `bench-internals` constructs a
// non-Scalar kernel directly.
pub trait CpuKernel: Copy + 'static {
    /// Mask the low `n` bits of `value`, returning the remaining
    /// high bits zeroed. The FSE bitstream hot path fires this 3×
    /// per decoded sequence; on BMI2-capable hardware this maps to
    /// a single `_bzhi_u64` instruction, otherwise to a scalar
    /// `u64::MAX >> (64 - n)` shift + mask.
    ///
    /// Precondition: `n <= 64`. Behaviour for `n == 0` is "return 0";
    /// behaviour for `n > 64` is unspecified — callers MUST uphold
    /// the bound. The test-only `mask_lower_bits` helper in
    /// `bit_reader_reverse.rs` debug-asserts the bound for its
    /// unit tests, but production callers (FSE / HUF hot paths)
    /// derive `n` from `accuracy_log` / `max_num_bits` which the
    /// per-stream table builders pin to `n <= MAX_*_BITS` at
    /// construction time; no per-call wrapper assert runs.
    fn mask_lower_bits(value: u64, n: u8) -> u64;

    /// Split the low `n1 + n2 + n3` bits of `packed` into three fields, the
    /// highest first. The FSE sequence decoder reads its three state updates
    /// this way, once per sequence.
    ///
    /// The default is three [`Self::mask_lower_bits`]; a kernel whose hardware
    /// extracts them in one instruction overrides it. Every implementation
    /// returns the same three values, so which one ran is invisible to the
    /// stream being decoded.
    ///
    /// Precondition: `n1 + n2 + n3 <= 64`, as for `mask_lower_bits`.
    #[inline(always)]
    fn extract_triple(packed: u64, n1: u8, n2: u8, n3: u8) -> (u64, u64, u64) {
        (
            Self::mask_lower_bits(packed.wrapping_shr(u32::from(n3) + u32::from(n2)), n1),
            Self::mask_lower_bits(packed.wrapping_shr(u32::from(n3)), n2),
            Self::mask_lower_bits(packed, n3),
        )
    }

    /// Width in bytes of [`Self::copy_chunks`]'s stores. The default is
    /// portable code, a machine word (`simd128` on wasm, which has no run-time
    /// tier); each SIMD tier overrides it with its own vector.
    const COPY_CHUNK: usize = crate::decoding::simd_copy::PORTABLE_COPY_CHUNK;

    /// Copy `len` bytes, a multiple of [`Self::COPY_CHUNK`], in whole chunks:
    /// the wildcopy body of the decoder's buffer copies.
    ///
    /// # Safety
    /// `src` readable and `dst` writable for `len` bytes; the regions do not
    /// overlap; the running CPU supports this kernel's tier.
    #[inline(always)]
    unsafe fn copy_chunks(src: *const u8, dst: *mut u8, len: usize) {
        unsafe { crate::decoding::simd_copy::copy_chunks_portable(src, dst, len) }
    }

    /// Width of [`Self::copy_step`], the chunk a wide tier steps down to when
    /// the slack left does not fit its own; equal to [`Self::COPY_CHUNK`]
    /// where there is nothing narrower to step to.
    const STEP_CHUNK: usize = Self::COPY_CHUNK;

    /// Copy `len` bytes, a multiple of [`Self::STEP_CHUNK`], in whole chunks.
    ///
    /// # Safety
    /// As [`Self::copy_chunks`].
    #[inline(always)]
    unsafe fn copy_step(src: *const u8, dst: *mut u8, len: usize) {
        unsafe { Self::copy_chunks(src, dst, len) }
    }

    /// Copy exactly 16 bytes in one transfer, for a short copy whose buffers
    /// have the room to overshoot to 16.
    ///
    /// # Safety
    /// `src` readable and `dst` writable for 16 bytes; the regions do not
    /// overlap; the running CPU supports this kernel's tier.
    #[inline(always)]
    unsafe fn copy16(src: *const u8, dst: *mut u8) {
        unsafe { crate::decoding::simd_copy::copy16_portable(src, dst) }
    }
}

/// Copies at the build's baseline vector (SSE2 where the target guarantees it,
/// NEON, `simd128`) and masks as the scalar tier does: for the copies that run
/// outside any tier's dispatch, a raw block's or a literals-only block's, which
/// every CPU the build runs on can take at that width.
#[derive(Copy, Clone, Default)]
pub(crate) struct BaselineKernel;

impl CpuKernel for BaselineKernel {
    #[inline(always)]
    fn mask_lower_bits(value: u64, n: u8) -> u64 {
        ScalarKernel::mask_lower_bits(value, n)
    }

    const COPY_CHUNK: usize = crate::decoding::simd_copy::BASELINE_COPY_CHUNK;

    #[inline(always)]
    unsafe fn copy_chunks(src: *const u8, dst: *mut u8, len: usize) {
        // SAFETY: the baseline runs on every CPU the build does.
        unsafe { crate::decoding::simd_copy::copy_chunks_baseline(src, dst, len) }
    }

    #[inline(always)]
    unsafe fn copy16(src: *const u8, dst: *mut u8) {
        // SAFETY: as above.
        unsafe { crate::decoding::simd_copy::copy16_baseline(src, dst) }
    }
}

/// The SSE2 copies every x86 SIMD tier shares: 16-byte chunks. A macro, so
/// each kernel's methods expand them in place.
#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-sse"
))]
macro_rules! sse2_copies {
    () => {
        const COPY_CHUNK: usize = 16;

        #[inline(always)]
        unsafe fn copy_chunks(src: *const u8, dst: *mut u8, len: usize) {
            // SAFETY: every tier using this was selected with SSE2 confirmed.
            unsafe { crate::decoding::simd_copy::copy_sse2(src, dst, len) }
        }

        #[inline(always)]
        unsafe fn copy16(src: *const u8, dst: *mut u8) {
            // SAFETY: as above.
            unsafe { crate::decoding::simd_copy::copy_sse2(src, dst, 16) }
        }
    };
}

/// x86 SSE2 kernel: scalar masks (SSE2 has no bit extract) and 16-byte SSE2
/// copies. Chosen at run time, so a 32-bit build whose baseline lacks SSE2
/// still copies in vectors on a CPU that has it.
#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-sse"
))]
#[derive(Copy, Clone, Default)]
pub(crate) struct Sse2Kernel;

#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-sse"
))]
impl CpuKernel for Sse2Kernel {
    #[inline(always)]
    fn mask_lower_bits(value: u64, n: u8) -> u64 {
        ScalarKernel::mask_lower_bits(value, n)
    }

    sse2_copies!();
}

/// Scalar fallback — portable, no SIMD or BMI2 intrinsics. Selected
/// when no x86 or aarch64 feature is detected at runtime.
#[derive(Copy, Clone, Default)]
pub struct ScalarKernel;

/// `BIT_MASK[n]` is the low `n` bits set for `n` in `0..=64`, and all bits for
/// anything past that (a width the formats cannot ask for).
///
/// A table rather than `u64::MAX >> (64 - n)`: the shift form needs a guard for
/// `n == 0`, since a 64-bit shift is undefined, and that guard is a branch or a
/// `cmov` on every field of every sequence. Indexed by a `u8`, and sized for
/// every `u8`, so the load carries no bounds check either. The widths in use
/// are small, so the hot part is the first few cache lines of it.
pub(crate) const BIT_MASK: [u64; 256] = {
    let mut table = [u64::MAX; 256];
    let mut i: usize = 0;
    while i < 64 {
        table[i] = (1u64 << i) - 1;
        i += 1;
    }
    table
};

impl CpuKernel for ScalarKernel {
    #[inline(always)]
    fn mask_lower_bits(value: u64, n: u8) -> u64 {
        value & BIT_MASK[n as usize]
    }
}

/// BMI2-only kernel: `bzhi` for mask_lower_bits. Selected when the CPU has
/// BMI2 but not the AVX2 SIMD width to upgrade to the Avx2 kernel. Treated as
/// a stepping stone between Sse2 and Avx2 on hardware that has BMI2 but not
/// AVX2 (rare in practice but matches upstream zstd's gating). Present on
/// 32-bit x86 as well as x86_64: the instruction is there, only its width
/// differs, and without this tier a 32-bit build would decode on the scalar
/// bodies whatever the CPU offers.
#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-bmi2"
))]
#[derive(Copy, Clone, Default)]
pub(crate) struct Bmi2Kernel;

#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-bmi2"
))]
impl CpuKernel for Bmi2Kernel {
    #[inline(always)]
    fn mask_lower_bits(value: u64, n: u8) -> u64 {
        // SAFETY: this kernel ZST is only reachable via the
        // `match detect_cpu_kernel() { CpuKernelTag::Bmi2 => ... }`
        // dispatch arms at decoder entry sites, all of which fire only
        // after `detect_cpu_kernel` confirmed BMI2 is available on the
        // running CPU.
        unsafe { mask_lower_bits_bmi2_impl(value, n) }
    }

    // Every BMI2 CPU has SSE2.
    sse2_copies!();
}

/// x86 AVX2 + BMI2 kernel (x86-64-v3 baseline). The common modern
/// x86 case — most CPUs released since 2013 (Haswell) have AVX2+BMI2.
/// Uses `bzhi` for mask ops and 256-bit moves for the buffer copies. Present
/// on 32-bit x86 too, where the kernel's masks are the table ones and its
/// copies are what it adds; the tier still decodes literals on the BMI2 path,
/// so, like every tier, it requires the ones below it.
#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-avx2"
))]
#[derive(Copy, Clone, Default)]
pub(crate) struct Avx2Kernel;

#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-avx2"
))]
impl CpuKernel for Avx2Kernel {
    #[inline(always)]
    fn mask_lower_bits(value: u64, n: u8) -> u64 {
        // SAFETY: Avx2Kernel is selected only after runtime detect
        // confirmed both AVX2 and BMI2 — `_bzhi_u64` is callable.
        #[cfg(target_arch = "x86_64")]
        unsafe {
            mask_lower_bits_bmi2_impl(value, n)
        }
        // 32-bit x86 has only a 32-bit `bzhi`: two of them and the branches
        // choosing between them cost more than the table lookup in the
        // sequence loop, which calls this three times a sequence (decoding
        // z000033 on i686 measured 12.6% slower at level 1 with them).
        #[cfg(target_arch = "x86")]
        {
            ScalarKernel::mask_lower_bits(value, n)
        }
    }

    // 32 bytes, the width the buffers' trailing slack is sized for.
    const COPY_CHUNK: usize = 32;

    #[inline(always)]
    unsafe fn copy_chunks(src: *const u8, dst: *mut u8, len: usize) {
        // SAFETY: this kernel is selected only after detect confirmed AVX2.
        unsafe { crate::decoding::simd_copy::copy_avx2(src, dst, len) }
    }

    const STEP_CHUNK: usize = 16;

    #[inline(always)]
    unsafe fn copy_step(src: *const u8, dst: *mut u8, len: usize) {
        // SAFETY: an AVX2 CPU has SSE2.
        unsafe { crate::decoding::simd_copy::copy_sse2(src, dst, len) }
    }

    #[inline(always)]
    unsafe fn copy16(src: *const u8, dst: *mut u8) {
        // SAFETY: as above.
        unsafe { crate::decoding::simd_copy::copy_sse2(src, dst, 16) }
    }
}

/// x86_64 AVX-512 VBMI2 + AVX2 + BMI2 kernel. Selected when the CPU
/// has the AVX-512 VBMI2 family available — VBMI2 unlocks a faster
/// HUF burst inner loop (VPSHUFB-based table lookup); BMI2 mask_lower
/// bits stays identical to Avx2 kernel.
#[cfg(all(target_arch = "x86_64", feature = "kernel-vbmi2"))]
#[derive(Copy, Clone, Default)]
pub(crate) struct Vbmi2Kernel;

#[cfg(all(target_arch = "x86_64", feature = "kernel-vbmi2"))]
impl CpuKernel for Vbmi2Kernel {
    #[inline(always)]
    fn mask_lower_bits(value: u64, n: u8) -> u64 {
        // SAFETY: same precondition as Avx2Kernel — BMI2 confirmed
        // at runtime before this kernel is instantiated.
        unsafe { mask_lower_bits_bmi2_impl(value, n) }
    }

    // The AVX2 width: a 64-byte store would need twice the buffers' trailing
    // slack to fire on the short copies that dominate.
    const COPY_CHUNK: usize = 32;

    #[inline(always)]
    unsafe fn copy_chunks(src: *const u8, dst: *mut u8, len: usize) {
        // SAFETY: the VBMI2 tier is selected only with AVX2 confirmed.
        unsafe { crate::decoding::simd_copy::copy_avx2(src, dst, len) }
    }

    const STEP_CHUNK: usize = 16;

    #[inline(always)]
    unsafe fn copy_step(src: *const u8, dst: *mut u8, len: usize) {
        // SAFETY: an AVX2 CPU has SSE2.
        unsafe { crate::decoding::simd_copy::copy_sse2(src, dst, len) }
    }

    #[inline(always)]
    unsafe fn copy16(src: *const u8, dst: *mut u8) {
        // SAFETY: as above.
        unsafe { crate::decoding::simd_copy::copy_sse2(src, dst, 16) }
    }
}

/// aarch64 NEON kernel: scalar masks and 16-byte NEON copies. Used on all
/// aarch64 hardware that exposes NEON (effectively universal on the
/// supported targets).
#[cfg(all(target_arch = "aarch64", feature = "kernel-neon"))]
#[derive(Copy, Clone, Default)]
pub(crate) struct NeonKernel;

#[cfg(all(target_arch = "aarch64", feature = "kernel-neon"))]
impl CpuKernel for NeonKernel {
    #[inline(always)]
    fn mask_lower_bits(value: u64, n: u8) -> u64 {
        // aarch64 has no BMI2 equivalent that improves on the scalar
        // shift-and-mask sequence for this op; the codegen is
        // identical to the Scalar kernel here.
        ScalarKernel::mask_lower_bits(value, n)
    }

    const COPY_CHUNK: usize = BaselineKernel::COPY_CHUNK;

    #[inline(always)]
    unsafe fn copy_chunks(src: *const u8, dst: *mut u8, len: usize) {
        // SAFETY: NEON is the aarch64 baseline.
        unsafe { BaselineKernel::copy_chunks(src, dst, len) }
    }

    #[inline(always)]
    unsafe fn copy16(src: *const u8, dst: *mut u8) {
        // SAFETY: as above.
        unsafe { BaselineKernel::copy16(src, dst) }
    }
}

/// aarch64 SVE kernel. Variable-vector-length SVE extends NEON for
/// HUF burst / SIMD copy on Graviton3 / Apple M-series with SVE
/// support. Masks and copies as the NEON kernel's.
#[cfg(all(target_arch = "aarch64", feature = "kernel-sve"))]
#[allow(dead_code)]
#[derive(Copy, Clone, Default)]
pub(crate) struct SveKernel;

#[cfg(all(target_arch = "aarch64", feature = "kernel-sve"))]
impl CpuKernel for SveKernel {
    #[inline(always)]
    fn mask_lower_bits(value: u64, n: u8) -> u64 {
        ScalarKernel::mask_lower_bits(value, n)
    }

    const COPY_CHUNK: usize = BaselineKernel::COPY_CHUNK;

    #[inline(always)]
    unsafe fn copy_chunks(src: *const u8, dst: *mut u8, len: usize) {
        // SAFETY: NEON is the aarch64 baseline.
        unsafe { BaselineKernel::copy_chunks(src, dst, len) }
    }

    #[inline(always)]
    unsafe fn copy16(src: *const u8, dst: *mut u8) {
        // SAFETY: as above.
        unsafe { BaselineKernel::copy16(src, dst) }
    }
}

/// Single `#[target_feature(enable = "bmi2")]` wrapper around the
/// `_bzhi_u64` intrinsic. Lifted to a free function so each kernel
/// impl that needs the BMI2 path (Bmi2 / Avx2 / Vbmi2) calls the
/// same shared body. With `#[inline]` LLVM inlines the call into
/// any caller that itself has BMI2 in scope; outside that scope the
/// target_feature boundary is preserved.
#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-bmi2"
))]
#[target_feature(enable = "bmi2")]
#[inline]
unsafe fn mask_lower_bits_bmi2_impl(value: u64, n: u8) -> u64 {
    // The intrinsic call is permitted directly inside a function
    // already annotated `#[target_feature(enable = "bmi2")]` — no
    // `unsafe { ... }` block needed (the function-level `unsafe`
    // already covers it). SAFETY: caller selected a kernel whose
    // CpuKernelTag was resolved after `is_x86_feature_detected!("bmi2")`
    // returned true, so the BMI2 instruction set is available.
    #[cfg(target_arch = "x86_64")]
    {
        core::arch::x86_64::_bzhi_u64(value, n as u32)
    }
    // 32-bit x86 has `bzhi` on 32-bit registers only. Widths up to 32 take one
    // instruction on the low half; wider ones keep the low 32 bits whole and
    // apply it to the high half, which is what a 64-bit `bzhi` does in one go.
    #[cfg(target_arch = "x86")]
    {
        use core::arch::x86::_bzhi_u32;
        if n >= 64 {
            return value;
        }
        if n <= 32 {
            return u64::from(_bzhi_u32(value as u32, u32::from(n)));
        }
        let high = _bzhi_u32((value >> 32) as u32, u32::from(n) - 32);
        (value & u64::from(u32::MAX)) | (u64::from(high) << 32)
    }
}

/// Pure boolean-input variant of the x86 kernel-tag selection. Both the
/// `std` runtime-detect path and the `no_std` compile-time-cfg path
/// route through this helper so the precedence rules stay in one place
/// (and are unit-testable without runtime CPUID).
///
/// The VBMI2 tier requires every AVX-512 sub-feature it touches AND the
/// AVX2 baseline — VBMI2 kernels mix VBMI2-only intrinsics with AVX2
/// 256-bit moves, so the dispatch must be conditioned on `has_avx2` too.
/// Likewise the Avx2 tier requires both AVX2 and BMI2.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
// Params go unused when the matching `kernel_*` feature is disabled (the
// rung that consumes them is `#[cfg]`-ed out); they are still passed by the
// detect callers. Silence the conditional unused-variable warning rather
// than thread per-feature `_`-prefixes through the signature.
#[allow(unused_variables)]
const fn select_x86_kernel(
    has_avx512vbmi2: bool,
    has_avx512f: bool,
    has_avx512vl: bool,
    has_avx512bw: bool,
    has_bmi2: bool,
    has_avx2: bool,
    has_sse2: bool,
) -> CpuKernelTag {
    #[cfg(feature = "kernel-vbmi2")]
    if has_avx512vbmi2 && has_avx512f && has_avx512vl && has_avx512bw && has_bmi2 && has_avx2 {
        return CpuKernelTag::Vbmi2;
    }
    #[cfg(feature = "kernel-avx2")]
    if has_avx2 && has_bmi2 {
        return CpuKernelTag::Avx2;
    }
    #[cfg(feature = "kernel-bmi2")]
    if has_bmi2 {
        return CpuKernelTag::Bmi2;
    }
    #[cfg(feature = "kernel-sse")]
    if has_sse2 {
        return CpuKernelTag::Sse2;
    }
    CpuKernelTag::Scalar
}

/// Cached runtime-detected kernel tag. The actual `CpuKernel` impl
/// (`ScalarKernel` / `Bmi2Kernel` / `Avx2Kernel` / `Vbmi2Kernel` /
/// `NeonKernel` / `SveKernel`) is constructed at the dispatch site —
/// currently only `decoding::literals_section_decoder::decompress_literals`
/// — via a `match` on this tag that branches into the per-K
/// `target_feature`-wrapped specialisation. Pipeline-wide dispatch
/// (FrameDecoder / FrameCompressor entry, sequence executor, match
/// copy) lands incrementally in follow-up tiers.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum CpuKernelTag {
    Scalar,
    /// Chosen at run time on 32-bit x86 too, where a build's baseline may
    /// lack SSE2 while the CPU has it.
    #[cfg(all(
        any(target_arch = "x86", target_arch = "x86_64"),
        feature = "kernel-sse"
    ))]
    Sse2,
    /// Reachable on 32-bit x86 as well: `bzhi` is there, and without the tier
    /// such a build would decode on the scalar bodies whatever the CPU offers.
    #[cfg(all(
        any(target_arch = "x86", target_arch = "x86_64"),
        feature = "kernel-bmi2"
    ))]
    Bmi2,
    /// On 32-bit x86 it runs the portable sequence walk with this kernel's
    /// 32-byte copies.
    #[cfg(all(
        any(target_arch = "x86", target_arch = "x86_64"),
        feature = "kernel-avx2"
    ))]
    Avx2,
    #[cfg(all(target_arch = "x86_64", feature = "kernel-vbmi2"))]
    Vbmi2,
    #[cfg(all(target_arch = "aarch64", feature = "kernel-neon"))]
    Neon,
    // Both constructors of `Sve` need a reachable feature: runtime
    // detection via `std::arch::is_aarch64_feature_detected!` (so
    // `feature = "std"`) or compile-time `target_feature = "sve"` in
    // RUSTFLAGS. Without either, the variant is unreachable and a
    // `match` arm referencing it warns as dead.
    #[cfg(all(
        target_arch = "aarch64",
        feature = "kernel-sve",
        any(feature = "std", target_feature = "sve"),
    ))]
    Sve,
}

/// Detect once and cache the best available CPU kernel for the
/// current process. Subsequent calls return the cached tag without
/// re-running CPU-feature detection. Std-only — no-std targets use
/// the compile-time variant below that resolves at build time.
#[cfg(feature = "std")]
pub(crate) fn detect_cpu_kernel() -> CpuKernelTag {
    static CACHED: OnceLock<CpuKernelTag> = OnceLock::new();
    *CACHED.get_or_init(detect_cpu_kernel_uncached)
}

#[cfg(feature = "std")]
fn detect_cpu_kernel_uncached() -> CpuKernelTag {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::is_x86_feature_detected;
        // Gate each probe on its tier feature: `cfg!(...)` const-folds, so the
        // `&&` short-circuits away the runtime `is_x86_feature_detected!` call
        // (and its CPUID/cache traffic) for tiers the build disabled — the
        // matching `select_x86_kernel` rung is `#[cfg]`-ed out anyway.
        let avx512 = cfg!(feature = "kernel-vbmi2") && cpu_allows(CpuLevel::Avx512);
        return select_x86_kernel(
            avx512 && is_x86_feature_detected!("avx512vbmi2"),
            avx512 && is_x86_feature_detected!("avx512f"),
            avx512 && is_x86_feature_detected!("avx512vl"),
            avx512 && is_x86_feature_detected!("avx512bw"),
            cfg!(feature = "kernel-bmi2")
                && cpu_allows(CpuLevel::Bmi2)
                && is_x86_feature_detected!("bmi2"),
            cfg!(feature = "kernel-avx2")
                && cpu_allows(CpuLevel::Avx2)
                && is_x86_feature_detected!("avx2"),
            cfg!(feature = "kernel-sse")
                && cpu_allows(CpuLevel::Sse2)
                && is_x86_feature_detected!("sse2"),
        );
    }
    // 32-bit x86 carries the SSE2, BMI2 and AVX2 tiers: SSE2 copies, `bzhi`
    // for the entropy tables and the AVX2 buffer copies. The VBMI2 bodies are
    // x86_64-only.
    #[cfg(target_arch = "x86")]
    {
        #[cfg(feature = "kernel-sse")]
        {
            use std::arch::is_x86_feature_detected;
            #[cfg(feature = "kernel-bmi2")]
            {
                let bmi2 = cpu_allows(CpuLevel::Bmi2) && is_x86_feature_detected!("bmi2");
                // The AVX2 tier sits above BMI2, as on x86_64: its literals take
                // the BMI2 body, so an AVX2 CPU without BMI2 (in practice a
                // virtual machine that hides it) runs the SSE2 tier. Its copies
                // stay 16 bytes wide; a tier of AVX2 copies without BMI2 would be
                // one more sequence decoder for a CPU no vendor ships.
                #[cfg(feature = "kernel-avx2")]
                if bmi2 && cpu_allows(CpuLevel::Avx2) && is_x86_feature_detected!("avx2") {
                    return CpuKernelTag::Avx2;
                }
                if bmi2 {
                    return CpuKernelTag::Bmi2;
                }
            }
            if cpu_allows(CpuLevel::Sse2) && is_x86_feature_detected!("sse2") {
                return CpuKernelTag::Sse2;
            }
        }
        return CpuKernelTag::Scalar;
    }
    #[cfg(target_arch = "aarch64")]
    {
        #[cfg(any(feature = "kernel-sve", feature = "kernel-neon"))]
        use std::arch::is_aarch64_feature_detected;
        #[cfg(feature = "kernel-sve")]
        if cpu_allows(CpuLevel::Sve) && is_aarch64_feature_detected!("sve") {
            return CpuKernelTag::Sve;
        }
        #[cfg(feature = "kernel-neon")]
        if cpu_allows(CpuLevel::Neon) && is_aarch64_feature_detected!("neon") {
            return CpuKernelTag::Neon;
        }
        return CpuKernelTag::Scalar;
    }
    #[allow(unreachable_code)]
    CpuKernelTag::Scalar
}

/// no-std variant: rely on compile-time `target_feature` flags
/// instead of runtime detection. Resolves to the most-capable kernel
/// that the build target supports.
#[cfg(not(feature = "std"))]
pub(crate) fn detect_cpu_kernel() -> CpuKernelTag {
    #[cfg(target_arch = "x86_64")]
    {
        // Route through the same const-fn precedence helper as the
        // `feature = "std"` path. `cfg!(target_feature = ...)`
        // returns a compile-time bool that constant-folds through
        // `select_x86_kernel`, so the runtime call has the same
        // codegen as the previous hand-written #[cfg] chain.
        let avx512 = cpu_allows(CpuLevel::Avx512);
        return select_x86_kernel(
            avx512 && cfg!(target_feature = "avx512vbmi2"),
            avx512 && cfg!(target_feature = "avx512f"),
            avx512 && cfg!(target_feature = "avx512vl"),
            avx512 && cfg!(target_feature = "avx512bw"),
            cpu_allows(CpuLevel::Bmi2) && cfg!(target_feature = "bmi2"),
            cpu_allows(CpuLevel::Avx2) && cfg!(target_feature = "avx2"),
            cpu_allows(CpuLevel::Sse2) && cfg!(target_feature = "sse2"),
        );
    }
    // `cfg!` rather than `#[cfg]`, as `select_x86_kernel` does: the tests fold
    // at compile time, and the tiers stay constructed in every build.
    #[cfg(all(target_arch = "x86", feature = "kernel-bmi2"))]
    {
        let bmi2 = cpu_allows(CpuLevel::Bmi2) && cfg!(target_feature = "bmi2");
        #[cfg(feature = "kernel-avx2")]
        if bmi2 && cpu_allows(CpuLevel::Avx2) && cfg!(target_feature = "avx2") {
            return CpuKernelTag::Avx2;
        }
        if bmi2 {
            return CpuKernelTag::Bmi2;
        }
    }
    #[cfg(all(target_arch = "x86", feature = "kernel-sse"))]
    if cpu_allows(CpuLevel::Sse2) && cfg!(target_feature = "sse2") {
        return CpuKernelTag::Sse2;
    }
    #[cfg(target_arch = "aarch64")]
    {
        #[cfg(all(feature = "kernel-sve", target_feature = "sve"))]
        if cpu_allows(CpuLevel::Sve) {
            return CpuKernelTag::Sve;
        }
        #[cfg(all(feature = "kernel-neon", target_feature = "neon"))]
        if cpu_allows(CpuLevel::Neon) {
            return CpuKernelTag::Neon;
        }
    }
    #[allow(unreachable_code)]
    CpuKernelTag::Scalar
}

impl CpuKernelTag {
    /// Stable lowercase diagnostic name for this tier (used by
    /// [`active_cpu_kernel_name`] and the bench/dashboard reporting). Pure
    /// mapping over the tag, so every arm is exercisable in tests regardless
    /// of which tier the running CPU actually resolves to.
    pub(crate) fn name(self) -> &'static str {
        match self {
            CpuKernelTag::Scalar => "scalar",
            #[cfg(all(
                any(target_arch = "x86", target_arch = "x86_64"),
                feature = "kernel-sse"
            ))]
            CpuKernelTag::Sse2 => "sse2",
            #[cfg(all(
                any(target_arch = "x86", target_arch = "x86_64"),
                feature = "kernel-bmi2"
            ))]
            CpuKernelTag::Bmi2 => "bmi2",
            #[cfg(all(
                any(target_arch = "x86", target_arch = "x86_64"),
                feature = "kernel-avx2"
            ))]
            CpuKernelTag::Avx2 => "avx2",
            #[cfg(all(target_arch = "x86_64", feature = "kernel-vbmi2"))]
            CpuKernelTag::Vbmi2 => "vbmi2",
            #[cfg(all(target_arch = "aarch64", feature = "kernel-neon"))]
            CpuKernelTag::Neon => "neon",
            #[cfg(all(
                target_arch = "aarch64",
                feature = "kernel-sve",
                any(feature = "std", target_feature = "sve"),
            ))]
            CpuKernelTag::Sve => "sve",
        }
    }
}

/// Name of the CPU kernel tier this process selected for the entropy /
/// sequence hot paths: decode (literals + FSE sequence decode) and encode
/// (entropy) share this dispatch (see #247). Returned as a stable lowercase
/// string for diagnostics and benchmark/dashboard reporting; the value is
/// what the runtime CPU-feature detection (or compile-time `target_feature`
/// on `no_std`) actually resolves to on this machine, so a dashboard can
/// attribute a measurement to the kernel that produced it.
pub fn active_cpu_kernel_name() -> &'static str {
    detect_cpu_kernel().name()
}

#[cfg(test)]
mod tests;
