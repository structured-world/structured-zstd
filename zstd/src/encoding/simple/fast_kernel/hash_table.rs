//! Flat `Vec<u32>` hash table used by the donor-shape Fast strategy
//! match-finder. Direct port of `ZSTD_hash4`/`ZSTD_hash5`/`ZSTD_hash6`/
//! `ZSTD_hash7`/`ZSTD_hash8` from
//! `lib/compress/zstd_compress_internal.h` — multiply-shift on the first
//! `mls` bytes of the suffix at `ptr`, keyed into a power-of-two table
//! sized `1 << hash_log` entries.

use alloc::vec;
use alloc::vec::Vec;

/// Donor `ZSTD_HASHLOG_MAX` (`lib/zstd.h`). The cap applies uniformly
/// across all four `mls` instantiations: even though `mls >= 5` widens
/// the hash to a `u64` reduction, the Fast strategy's per-level
/// `hashLog` is sourced from the donor's `ZSTD_defaultCParameters`
/// table where the maximum is `14` (level 1, `srcSize > 256 KB`), and
/// the user-tunable upper bound is `30`. Enforcing this in the
/// constructor catches misuse before the first `hash_ptr` would
/// otherwise panic on the `(32 - hash_log)` / `(64 - hash_log)` shift.
const ZSTD_HASHLOG_MAX: u32 = 30;

/// Donor multiplicative hash constants — exact bit-for-bit match with
/// `lib/compress/zstd_compress_internal.h` so the table-keying behaviour
/// stays identical to the reference encoder.
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
    /// Panics if `hash_log` is outside `1..=ZSTD_HASHLOG_MAX` (donor's
    /// cap, currently `30`). The lower bound exists because `0` would
    /// make `hash_ptr` shift by the full word width (`32` for mls=4,
    /// `64` for mls≥5) — UB / panic in Rust. The upper bound is the
    /// donor's documented maximum; importantly, even on 64-bit
    /// targets a `usize::BITS - 1` cap would still admit `hash_log
    /// ∈ 33..=63` which is invalid for the `mls=4` path that shifts
    /// by `32 - hash_log` (panics for `hash_log >= 32`). Pinning to
    /// `ZSTD_HASHLOG_MAX` rejects both invalid bands at construction
    /// time so every subsequent `hash_ptr::<MLS>` call is safe by
    /// construction.
    ///
    /// Also panics if `mls` is outside `4..=8`.
    pub(crate) fn new(hash_log: u32, mls: u32) -> Self {
        assert!(
            (1..=ZSTD_HASHLOG_MAX).contains(&hash_log),
            "hash_log must be in 1..={ZSTD_HASHLOG_MAX} for donor-compatible Fast hashing (got {hash_log}); \
             the lower bound prevents a full-word-width shift in hash_ptr, the upper bound is donor's ZSTD_HASHLOG_MAX",
        );
        assert!(
            (4..=8).contains(&mls),
            "ZSTD Fast strategy only supports mls 4..=8 (got {mls})",
        );
        Self {
            table: vec![0u32; 1usize << hash_log],
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
    /// `ptr` MUST point to readable bytes covering the load width:
    /// - `MLS == 4`: at least **4** readable bytes (a `u32` load).
    /// - `MLS >= 5`: at least **8** readable bytes — every mls ∈ {5,
    ///   6, 7, 8} path performs an unaligned `u64::read_unaligned`
    ///   and shifts off the unused top bits, so the underlying load
    ///   is always 8 bytes wide regardless of `mls`. Promising only
    ///   `mls` readable bytes for `mls ∈ {5,6,7}` would leave the
    ///   trailing 8-mls bytes of the u64 read past the caller's
    ///   range — UB.
    ///
    /// The kernel satisfies this uniformly via the
    /// `ilimit = iend - HASH_READ_SIZE` cap (`HASH_READ_SIZE = 8`),
    /// mirroring donor's same invariant.
    #[inline(always)]
    pub(crate) unsafe fn hash_ptr<const MLS: u32>(&self, ptr: *const u8) -> u32 {
        debug_assert_eq!(MLS, self.mls, "monomorphised MLS must match table mls");
        match MLS {
            4 => {
                // SAFETY: caller guarantees ≥4 readable bytes at ptr.
                let u = unsafe { core::ptr::read_unaligned(ptr.cast::<u32>()) }.to_le();
                u.wrapping_mul(PRIME_4_BYTES) >> (32 - self.hash_log)
            }
            5 => {
                // SAFETY: caller guarantees ≥5 readable bytes; the
                // u64 load reads 8 but only the bottom 40 bits are
                // hashed (`<< (64-40)` shifts the rest off).
                let u = unsafe { core::ptr::read_unaligned(ptr.cast::<u64>()) }.to_le();
                ((u << (64 - 40)).wrapping_mul(PRIME_5_BYTES) >> (64 - self.hash_log)) as u32
            }
            6 => {
                // SAFETY: caller guarantees ≥6 readable bytes; same
                // u64-load + top-bit shift pattern as mls=5.
                let u = unsafe { core::ptr::read_unaligned(ptr.cast::<u64>()) }.to_le();
                ((u << (64 - 48)).wrapping_mul(PRIME_6_BYTES) >> (64 - self.hash_log)) as u32
            }
            7 => {
                // SAFETY: caller guarantees ≥7 readable bytes.
                let u = unsafe { core::ptr::read_unaligned(ptr.cast::<u64>()) }.to_le();
                ((u << (64 - 56)).wrapping_mul(PRIME_7_BYTES) >> (64 - self.hash_log)) as u32
            }
            8 => {
                // SAFETY: caller guarantees ≥8 readable bytes — the
                // donor reads the full u64 unchanged for mls=8.
                let u = unsafe { core::ptr::read_unaligned(ptr.cast::<u64>()) }.to_le();
                (u.wrapping_mul(PRIME_8_BYTES) >> (64 - self.hash_log)) as u32
            }
            _ => {
                // Compile-time unreachable for monomorphised callers;
                // emitting an `unreachable_unchecked()` here would be
                // UB in debug builds if anyone instantiates a bad MLS.
                debug_assert!(false, "unsupported MLS {MLS}");
                0
            }
        }
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
}
