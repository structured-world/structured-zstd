//! Flat `Vec<u32>` hash table used by the donor-shape Fast strategy
//! match-finder. Direct port of `ZSTD_hash4`/`ZSTD_hash5`/`ZSTD_hash6`/
//! `ZSTD_hash7`/`ZSTD_hash8` from
//! `lib/compress/zstd_compress_internal.h` — multiply-shift on the first
//! `mls` bytes of the suffix at `ptr`, keyed into a power-of-two table
//! sized `1 << hash_log` entries.

use alloc::vec;
use alloc::vec::Vec;

/// Donor `ZSTD_HASHLOG_MAX` (`lib/zstd.h`). The cap applies uniformly
/// across all five `mls` instantiations (`mls ∈ {4, 5, 6, 7, 8}`): even
/// though `mls >= 5` widens the hash to a `u64` reduction, the Fast
/// strategy's per-level `hashLog` is sourced from the donor's
/// `ZSTD_defaultCParameters` table where the maximum is `14` (level 1,
/// `srcSize > 256 KB`), and the user-tunable upper bound is `30`.
/// Enforcing this in the constructor catches misuse before the first
/// `hash_ptr` would otherwise panic on the `(32 - hash_log)` /
/// `(64 - hash_log)` shift.
const ZSTD_HASHLOG_MAX: u32 = 30;

/// Donor multiplicative hash constants — exact bit-for-bit match with
/// `lib/compress/zstd_compress_internal.h` so the table-keying behaviour
/// stays identical to the reference encoder.
/// Non-allocating parameter validation shared by [`FastHashTable::new`] and
/// the constructor-accept-path tests. Extracted so tests can prove the
/// `(1..=ZSTD_HASHLOG_MAX, 4..=8)` accept band without forcing a
/// `1 << ZSTD_HASHLOG_MAX` allocation (≈4 GiB at `ZSTD_HASHLOG_MAX = 30`,
/// well above per-test memory budgets on CI runners). Panics with the
/// same messages the [`FastHashTable::new`] doc-comment cites.
fn validate_params(hash_log: u32, mls: u32) {
    assert!(
        (1..=ZSTD_HASHLOG_MAX).contains(&hash_log),
        "hash_log must be in 1..={ZSTD_HASHLOG_MAX} for donor-compatible Fast hashing (got {hash_log}); \
         the lower bound prevents a full-word-width shift in hash_ptr, the upper bound is donor's ZSTD_HASHLOG_MAX",
    );
    assert!(
        (4..=8).contains(&mls),
        "ZSTD Fast strategy only supports mls 4..=8 (got {mls})",
    );
}

const PRIME_4_BYTES: u32 = 0x9E3779B1;
const PRIME_5_BYTES: u64 = 889_523_592_379;
const PRIME_6_BYTES: u64 = 227_718_039_650_203;
const PRIME_7_BYTES: u64 = 58_295_818_150_454_627;
const PRIME_8_BYTES: u64 = 0xCF1BBCDCB7A56463;

/// Flat hash table indexed by `hash_ptr(ptr, hash_log, mls)`. Entries
/// store absolute positions into the encoder's flat history buffer
/// (matches donor's `U32* hashTable` with `base + matchIdx` lookup).
/// Sentinel `0` is fine because position `0` either belongs to the
/// initial prefix (where the `+= (ip0 == prefixStart)` adjustment at
/// loop entry skips it) or is below `prefixStartIndex` and filtered by
/// the in-range check.
#[derive(Clone)]
pub(crate) struct FastHashTable {
    table: Vec<u32>,
    /// Donor `hash_log` — number of bits the hash output is reduced to.
    hash_log: u32,
    /// Donor `mls` — minimum match length used as the hash input width.
    /// Valid range `4..=8`; the kernel monomorphises over this so it
    /// compiles to a constant inside each instantiation.
    mls: u32,
}

impl FastHashTable {
    /// Allocate the table at `1 << hash_log` entries, all initialised
    /// to the sentinel `0` position. The encoder is expected to bump
    /// the first real input position to at least `1` so the sentinel
    /// can never be confused with a valid match (the donor achieves
    /// this via `ip0 += (ip0 == prefixStart)`).
    ///
    /// # Panics
    ///
    /// Parameter-range failures:
    /// - `hash_log` outside `1..=ZSTD_HASHLOG_MAX` (donor's cap,
    ///   currently `30`). The lower bound exists because `0` would
    ///   make `hash_ptr` shift by the full word width (`32` for
    ///   mls=4, `64` for mls≥5) — UB / panic in Rust. The upper
    ///   bound is the donor's documented maximum; importantly,
    ///   even on 64-bit targets a `usize::BITS - 1` cap would still
    ///   admit `hash_log ∈ 33..=63` which is invalid for the
    ///   `mls=4` path that shifts by `32 - hash_log` (panics for
    ///   `hash_log >= 32`). Pinning to `ZSTD_HASHLOG_MAX` rejects
    ///   both invalid bands at construction time so every
    ///   subsequent `hash_ptr::<MLS>` call is safe by construction.
    /// - `mls` outside `4..=8`.
    ///
    /// Target-size / allocation failures (per-host, not per-input):
    /// - `1usize << hash_log` overflowing `usize`. Only reachable on
    ///   32-bit hosts at `hash_log >= 32` — but `validate_params`
    ///   already pins `hash_log <= ZSTD_HASHLOG_MAX = 30`, so this
    ///   path is unreachable today for the donor-compatible band.
    ///   Kept here as a tripwire if `ZSTD_HASHLOG_MAX` is ever
    ///   raised past `31`.
    /// - `entries * size_of::<u32>()` overflowing `usize`. This is
    ///   the deterministic 32-bit failure mode at `hash_log = 30`:
    ///   `1 << 30` entries × 4 bytes = 4 GiB, which is the full
    ///   32-bit address space and overflows the `usize` multiply
    ///   before `vec![]` is even called. The `checked_mul` guard
    ///   surfaces this as a clear panic instead of the opaque
    ///   `Vec`-internal capacity-overflow message.
    /// - Global allocator failure when actually allocating the
    ///   table backing storage — propagates as the standard
    ///   `Vec::with_capacity` allocation-failure panic.
    ///
    /// The first two guards fire BEFORE control reaches `vec![]`
    /// and are deterministic given the inputs and target
    /// architecture. The third bullet IS the `vec![]` allocation
    /// itself — it's the only panic that depends on runtime memory
    /// state.
    pub(crate) fn new(hash_log: u32, mls: u32) -> Self {
        validate_params(hash_log, mls);
        // Per-target allocation feasibility: `1 << hash_log` u32 entries
        // = `1 << (hash_log + 2)` bytes. On 32-bit hosts that overflows
        // `usize` at `hash_log >= 30` (4 GiB exceeds the address space).
        // `validate_params` already pins `hash_log <= ZSTD_HASHLOG_MAX
        // = 30`, but on 32-bit the maximum that actually fits is `<=
        // 29` (2 GiB) — anything larger panics deep inside `Vec::with_
        // capacity` with a generic allocation message. Surface a clear
        // panic at construction so the failure mode is obvious instead.
        let entries = 1usize.checked_shl(hash_log).unwrap_or_else(|| {
            panic!(
                "FastHashTable cannot allocate 2^{hash_log} u32 entries on this target: \
                 `1usize << {hash_log}` overflows {0}-bit usize",
                usize::BITS,
            )
        });
        let bytes = entries
            .checked_mul(core::mem::size_of::<u32>())
            .unwrap_or_else(|| {
                panic!(
                    "FastHashTable cannot allocate {entries} u32 entries on this target: \
                 byte size overflows {0}-bit usize",
                    usize::BITS,
                )
            });
        // Use `bytes` to compute as a tripwire — actual allocation
        // still goes through `vec![]` so the global allocator picks
        // the strategy (zeroed page mapping, etc.).
        let _ = bytes;
        Self {
            table: vec![0u32; entries],
            hash_log,
            mls,
        }
    }

    #[inline(always)]
    pub(crate) fn hash_log(&self) -> u32 {
        self.hash_log
    }

    #[inline(always)]
    pub(crate) fn mls(&self) -> u32 {
        self.mls
    }

    /// Clear the table back to all-sentinel. Used on encoder reset
    /// between independent frames so a stale absolute index from the
    /// previous frame can't get mistaken for a current-frame match.
    pub(crate) fn clear(&mut self) {
        // `fill(0)` lowers to a single `memset` and is significantly
        // faster than re-allocating; the table can be hundreds of KiB.
        self.table.fill(0);
    }

    /// Donor-parity `ZSTD_hashPtr` — multiply-shift hash over the first
    /// `mls` bytes at `ptr`, output reduced to `hash_log` bits.
    ///
    /// # Safety
    ///
    /// **Readable-bytes contract on `ptr`:**
    /// - `MLS == 4`: at least **4** readable bytes (a `u32` load).
    /// - `MLS >= 5`: at least **8** readable bytes — every mls ∈ {5,
    ///   6, 7, 8} path performs an unaligned `u64::read_unaligned`
    ///   and shifts off the unused top bits, so the underlying load
    ///   is always 8 bytes wide regardless of `mls`. Promising only
    ///   `mls` readable bytes for `mls ∈ {5,6,7}` would leave the
    ///   trailing 8-mls bytes of the u64 read past the caller's
    ///   range — UB.
    ///
    /// The kernel satisfies the readable-bytes promise uniformly
    /// via the `ilimit = iend - HASH_READ_SIZE` cap
    /// (`HASH_READ_SIZE = 8`), mirroring donor's same invariant.
    ///
    /// **`MLS` const-generic contract:**
    /// - `MLS` MUST equal `self.mls()`.
    /// - `MLS` MUST be in `4..=8`.
    ///
    /// Today only `debug_assert_eq!` checks the equality at runtime,
    /// so in release a mismatch silently routes to the wrong hash
    /// formula (different multiply prime, different shift width)
    /// and probes entries indexed by a different formula — garbage
    /// match candidates. Callers must guarantee both invariants
    /// before invoking `hash_ptr`. The crate-internal entry point
    /// [`crate::encoding::simple::fast_kernel::kernel::compress_block_fast`]
    /// enforces both via real `assert!`s before any `hash_ptr` call,
    /// so invocation through the kernel is safe by construction;
    /// direct callers (tests, future helpers) must uphold the
    /// contract themselves.
    #[inline(always)]
    pub(crate) unsafe fn hash_ptr<const MLS: u32>(&self, ptr: *const u8) -> u32 {
        debug_assert_eq!(MLS, self.mls, "monomorphised MLS must match table mls");
        // SAFETY: forwarded — caller upholds `hash_ptr`'s readable-bytes
        // contract; `self.hash_log` is the table's own log.
        unsafe { hash_ptr_raw::<MLS>(ptr, self.hash_log) }
    }

    /// Hoist the table's backing slice + `hash_log` into locals for a hot
    /// loop. Binding `&mut [u32]` to a local caches the `(ptr, len)` once, so
    /// per-position `get_unchecked` / `get_unchecked_mut` don't reload the
    /// `Vec` header on every access — the optimiser otherwise conservatively
    /// re-reads it through `&mut FastHashTable` after each interior write
    /// (the "chases the Vec" reload). Pair with [`hash_ptr_raw`] so the loop
    /// never touches `self` and stays reload-free.
    #[inline(always)]
    pub(crate) fn hot_state(&mut self) -> (&mut [u32], u32) {
        (self.table.as_mut_slice(), self.hash_log)
    }

    /// Direct table access — `table[hash]`. Bounds-check at index time
    /// is provably redundant because `hash >> (64 - hash_log)` produces
    /// a value `< 1 << hash_log == table.len()`; LLVM cannot infer
    /// this across the `as u32` truncation so we use `get_unchecked`.
    ///
    /// # Safety
    ///
    /// `hash` MUST be a value returned by [`hash_ptr`] on this table
    /// (or on another table with the same `hash_log`), so that
    /// `hash < 1 << hash_log = table.len()`.
    #[inline(always)]
    pub(crate) unsafe fn get(&self, hash: u32) -> u32 {
        debug_assert!((hash as usize) < self.table.len());
        // SAFETY: see method-level doc — `hash` is bounded by the
        // table-size invariant from `hash_ptr`.
        unsafe { *self.table.get_unchecked(hash as usize) }
    }

    /// Direct table write — `table[hash] = pos`. Same bounds reasoning
    /// as [`get`].
    ///
    /// # Safety
    ///
    /// `hash` MUST be a value returned by [`hash_ptr`] on this table.
    #[inline(always)]
    pub(crate) unsafe fn put(&mut self, hash: u32, pos: u32) {
        debug_assert!((hash as usize) < self.table.len());
        // SAFETY: see method-level doc.
        unsafe {
            *self.table.get_unchecked_mut(hash as usize) = pos;
        }
    }
}

/// Free-function form of [`FastHashTable::hash_ptr`] taking `hash_log`
/// explicitly so a hot loop can hoist it into a register once (via
/// [`FastHashTable::hot_state`]) instead of re-reading `self.hash_log` per
/// hash. Bit-for-bit identical to the method.
///
/// # Safety
/// Same readable-bytes contract as [`FastHashTable::hash_ptr`]: `MLS == 4`
/// needs ≥4 readable bytes at `ptr`, `MLS >= 5` needs ≥8. `hash_log` must be
/// in `1..=30` (the constructor's accepted band) so the shift is well-defined.
#[inline(always)]
pub(crate) unsafe fn hash_ptr_raw<const MLS: u32>(ptr: *const u8, hash_log: u32) -> u32 {
    match MLS {
        4 => {
            // SAFETY: caller guarantees ≥4 readable bytes at ptr.
            let u = unsafe { core::ptr::read_unaligned(ptr.cast::<u32>()) }.to_le();
            u.wrapping_mul(PRIME_4_BYTES) >> (32 - hash_log)
        }
        5 => {
            // SAFETY: caller guarantees ≥8 readable bytes (wide u64 load).
            let u = unsafe { core::ptr::read_unaligned(ptr.cast::<u64>()) }.to_le();
            ((u << (64 - 40)).wrapping_mul(PRIME_5_BYTES) >> (64 - hash_log)) as u32
        }
        6 => {
            // SAFETY: caller guarantees ≥8 readable bytes (u64 load).
            let u = unsafe { core::ptr::read_unaligned(ptr.cast::<u64>()) }.to_le();
            ((u << (64 - 48)).wrapping_mul(PRIME_6_BYTES) >> (64 - hash_log)) as u32
        }
        7 => {
            // SAFETY: caller guarantees ≥8 readable bytes (u64 load).
            let u = unsafe { core::ptr::read_unaligned(ptr.cast::<u64>()) }.to_le();
            ((u << (64 - 56)).wrapping_mul(PRIME_7_BYTES) >> (64 - hash_log)) as u32
        }
        8 => {
            // SAFETY: caller guarantees ≥8 readable bytes (full u64).
            let u = unsafe { core::ptr::read_unaligned(ptr.cast::<u64>()) }.to_le();
            (u.wrapping_mul(PRIME_8_BYTES) >> (64 - hash_log)) as u32
        }
        _ => {
            debug_assert!(false, "unsupported MLS {MLS}");
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Donor parity: `ZSTD_hash4` on `[0x01, 0x02, 0x03, 0x04]` with
    /// hash_log=12 produces a specific bit pattern. Captured here as a
    /// regression tripwire so any future refactor of the multiply
    /// constants surfaces immediately.
    #[test]
    fn hash4_matches_donor_formula_on_known_input() {
        let table = FastHashTable::new(12, 4);
        let data = [0x01u8, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        // SAFETY: data has 8 ≥ 4 readable bytes.
        let h = unsafe { table.hash_ptr::<4>(data.as_ptr()) };
        // Manual donor calc: u32::from_le_bytes(0x04030201) * 0x9E3779B1 >> 20.
        let expected = 0x04030201u32.wrapping_mul(0x9E3779B1) >> 20;
        assert_eq!(h, expected, "hash4 must match donor multiply-shift formula");
    }

    #[test]
    fn hash5_matches_donor_formula_on_known_input() {
        let table = FastHashTable::new(13, 5);
        let data = [0x01u8, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        // SAFETY: data has 8 ≥ 5 readable bytes.
        let h = unsafe { table.hash_ptr::<5>(data.as_ptr()) };
        let u = u64::from_le_bytes(data);
        let expected = (((u << (64 - 40)).wrapping_mul(889_523_592_379u64)) >> (64 - 13)) as u32;
        assert_eq!(h, expected);
    }

    #[test]
    fn get_put_round_trip_under_known_hash() {
        let mut table = FastHashTable::new(8, 4);
        let data = [0xAAu8, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11];
        // SAFETY: data has 8 ≥ 4 readable bytes.
        let h = unsafe { table.hash_ptr::<4>(data.as_ptr()) };
        // SAFETY: h came from hash_ptr on this table.
        unsafe {
            assert_eq!(table.get(h), 0, "fresh table reads sentinel");
            table.put(h, 0xCAFE_BABE);
            assert_eq!(table.get(h), 0xCAFE_BABE);
        }
    }

    #[test]
    fn clear_resets_all_entries_to_sentinel() {
        let mut table = FastHashTable::new(6, 4);
        let data = [1u8, 2, 3, 4, 5, 6, 7, 8];
        // SAFETY: 4 readable bytes.
        let h = unsafe { table.hash_ptr::<4>(data.as_ptr()) };
        // SAFETY: hash came from hash_ptr.
        unsafe {
            table.put(h, 42);
        }
        table.clear();
        // SAFETY: hash came from hash_ptr.
        let read_back = unsafe { table.get(h) };
        assert_eq!(read_back, 0, "clear must zero every entry");
    }

    /// Donor parity for mls=6: `ZSTD_hash6` uses
    /// `(u << (64-48)).wrapping_mul(PRIME_6_BYTES) >> (64 - hash_log)`.
    #[test]
    fn hash6_matches_donor_formula_on_known_input() {
        let table = FastHashTable::new(14, 6);
        let data = [0x01u8, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        // SAFETY: data has 8 ≥ 6 readable bytes (the implementation
        // performs a u64 load and shifts off the unused top bits).
        let h = unsafe { table.hash_ptr::<6>(data.as_ptr()) };
        let u = u64::from_le_bytes(data);
        let expected =
            (((u << (64 - 48)).wrapping_mul(227_718_039_650_203u64)) >> (64 - 14)) as u32;
        assert_eq!(h, expected, "hash6 must match donor multiply-shift formula");
    }

    /// Donor parity for mls=7: `ZSTD_hash7` shifts by `(64-56)` and
    /// multiplies by `PRIME_7_BYTES`.
    #[test]
    fn hash7_matches_donor_formula_on_known_input() {
        let table = FastHashTable::new(15, 7);
        let data = [0x01u8, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        // SAFETY: data has 8 ≥ 7 readable bytes.
        let h = unsafe { table.hash_ptr::<7>(data.as_ptr()) };
        let u = u64::from_le_bytes(data);
        let expected =
            (((u << (64 - 56)).wrapping_mul(58_295_818_150_454_627u64)) >> (64 - 15)) as u32;
        assert_eq!(h, expected, "hash7 must match donor multiply-shift formula");
    }

    /// Donor parity for mls=8: `ZSTD_hash8` does NOT shift the input
    /// (full u64), then multiplies by `PRIME_8_BYTES` (donor's
    /// `prime8bytes = 0xCF1BBCDCB7A56463ULL`).
    #[test]
    fn hash8_matches_donor_formula_on_known_input() {
        let table = FastHashTable::new(16, 8);
        let data = [0x01u8, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        // SAFETY: data has 8 readable bytes.
        let h = unsafe { table.hash_ptr::<8>(data.as_ptr()) };
        let u = u64::from_le_bytes(data);
        let expected = (u.wrapping_mul(0xCF1BBCDCB7A56463u64) >> (64 - 16)) as u32;
        assert_eq!(h, expected, "hash8 must match donor multiply-shift formula");
    }

    /// Boundary: `hash_log = 1` is the smallest accepted value
    /// (table of 2 entries). Verifies the constructor doesn't reject
    /// the minimum and that the table actually allocates.
    #[test]
    fn hash_log_minimum_one_constructs_two_entry_table() {
        let table = FastHashTable::new(1, 4);
        let data = [0u8, 0, 0, 0];
        // SAFETY: 4 readable bytes; hash output occupies 1 bit so
        // hash ∈ {0, 1}.
        let h = unsafe { table.hash_ptr::<4>(data.as_ptr()) };
        assert!(h < 2, "hash_log=1 must produce values ∈ {{0, 1}} (got {h})");
    }

    /// Boundary: `hash_log = ZSTD_HASHLOG_MAX` (30) is the largest
    /// accepted value. Calling [`FastHashTable::new`] with this value
    /// would allocate ≈4 GiB (`1 << 30` × `sizeof(u32)`), well over
    /// per-test memory budgets on CI runners — instead exercise the
    /// same accept path via the non-allocating [`validate_params`]
    /// helper. Pairs with the `should_panic` tests below that prove
    /// rejection of the out-of-band cases.
    #[test]
    fn hash_log_maximum_thirty_is_accepted_by_validation() {
        validate_params(ZSTD_HASHLOG_MAX, 4);
        validate_params(ZSTD_HASHLOG_MAX, 8);
    }

    #[test]
    #[should_panic(expected = "hash_log must be in 1..=")]
    fn panics_on_zero_hash_log() {
        let _ = FastHashTable::new(0, 4);
    }

    #[test]
    #[should_panic(expected = "hash_log must be in 1..=")]
    fn panics_on_hash_log_above_zstd_hashlog_max() {
        // 31 > ZSTD_HASHLOG_MAX (30) → constructor rejects.
        let _ = FastHashTable::new(31, 4);
    }

    #[test]
    #[should_panic(expected = "ZSTD Fast strategy only supports mls 4..=8")]
    fn panics_on_mls_below_four() {
        let _ = FastHashTable::new(12, 3);
    }

    #[test]
    #[should_panic(expected = "ZSTD Fast strategy only supports mls 4..=8")]
    fn panics_on_mls_above_eight() {
        let _ = FastHashTable::new(12, 9);
    }
}
