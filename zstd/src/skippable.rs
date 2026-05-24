//! Typed Rust API for zstd skippable frames (RFC 8878 §3.1).
//!
//! Skippable frames carry an arbitrary application payload alongside
//! a zstd data stream. Spec layout, byte-compatible with donor
//! `ZSTD_writeSkippableFrame`
//! (`lib/compress/zstd_compress.c:4751-4763` in zstd v1.5.7):
//!
//! ```text
//! +----------+-----------+----------------+
//! | 4 bytes  | 4 bytes   | payload bytes  |
//! | magic LE | length LE | (size = length)|
//! +----------+-----------+----------------+
//! ```
//!
//! - `magic = 0x184D2A50 + magic_variant`, with `magic_variant` in
//!   `0..=15` — 16 application-claimed magic numbers in the
//!   skippable-magic range `0x184D2A50..=0x184D2A5F`.
//! - `length` is the payload byte count as a little-endian `u32`,
//!   so payloads above `u32::MAX` are not representable on the wire
//!   (the validation in [`SkippableFrame::new`] / [`write_skippable_frame`]
//!   surfaces this as [`SkippableFrameError::PayloadTooLarge`]).
//!
//! # Primary use case
//!
//! Embedded metadata sidecars in storage formats. The first canonical
//! consumer is the lsm-tree v1 encrypted wire format
//! (<https://github.com/structured-world/coordinode-lsm-tree>), which
//! stacks `MetadataFrame` / `BodyFrame` / `EccFrame` skippable frames
//! around an inner zstd frame. Any storage-format author needing to
//! interleave metadata with zstd data can use the same shape — the
//! API takes a generic `magic_variant: u8` and leaves the per-variant
//! semantics to the application.
//!
//! # Magic variant allocation policy
//!
//! Magic variants `0x184D2A50..=0x184D2A5F` are an **application-protocol**
//! concern, NOT a structured-zstd concern. This crate accepts
//! `magic_variant: u8` in `0..=15` and validates only that bound. No
//! per-variant constants are baked into the source — applications are
//! responsible for documenting which variants they claim and
//! coordinating with other ecosystem consumers to avoid collisions.

extern crate alloc;

use alloc::vec::Vec;

use crate::io::{Error, Read, Write};

/// First magic number in the skippable-frame range (RFC 8878 §3.1.2).
/// Variants 0..=15 correspond to magics in `[0x184D2A50, 0x184D2A5F]`.
pub const SKIPPABLE_MAGIC_START: u32 = 0x184D_2A50;

/// Number of bytes the skippable-frame header occupies on the wire:
/// 4 bytes magic + 4 bytes length.
pub const SKIPPABLE_HEADER_SIZE: usize = 8;

/// Upper bound on the variant nibble. Variants are constrained to the
/// low 4 bits of the magic number so [`SKIPPABLE_MAGIC_START`] +
/// `variant` stays inside the spec's `0x184D2A50..=0x184D2A5F` band.
pub const SKIPPABLE_MAGIC_MAX_VARIANT: u8 = 15;

/// A typed skippable-frame value.
///
/// Construct via [`SkippableFrame::new`] (validates the variant bound
/// and payload size up front) or [`SkippableFrame::decode_from`].
/// Round-trip a frame via [`SkippableFrame::encode_into`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippableFrame {
    magic_variant: u8,
    payload: Vec<u8>,
}

impl SkippableFrame {
    /// Build a `SkippableFrame` from its components. Validates:
    /// - `magic_variant <= 15`
    ///   ([`SkippableFrameError::InvalidMagicVariant`]).
    /// - `payload.len() <= u32::MAX as usize`
    ///   ([`SkippableFrameError::PayloadTooLarge`]) — unreachable on
    ///   32-bit and smaller targets but enforced uniformly so 64-bit
    ///   callers cannot smuggle through an overlong payload.
    pub fn new(magic_variant: u8, payload: Vec<u8>) -> Result<Self, SkippableFrameError> {
        validate_magic_variant(magic_variant)?;
        validate_payload_size(payload.len())?;
        Ok(Self {
            magic_variant,
            payload,
        })
    }

    /// The 4-bit variant nibble. Combined with [`SKIPPABLE_MAGIC_START`]
    /// to form the on-wire magic number (`magic = START + variant`).
    pub fn magic_variant(&self) -> u8 {
        self.magic_variant
    }

    /// Full 32-bit magic number this frame serialises with.
    pub fn magic_number(&self) -> u32 {
        SKIPPABLE_MAGIC_START + u32::from(self.magic_variant)
    }

    /// Payload bytes carried by the frame (without the 8-byte header).
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Move the payload out, consuming the frame.
    pub fn into_payload(self) -> Vec<u8> {
        self.payload
    }

    /// Total serialised size of this frame on the wire:
    /// `payload.len() + 8` (8 = 4-byte magic + 4-byte length).
    pub fn serialized_size(&self) -> usize {
        self.payload.len() + SKIPPABLE_HEADER_SIZE
    }

    /// Serialise this frame into `writer`. Writes
    /// `serialized_size()` bytes total: 4-byte magic LE,
    /// 4-byte length LE, payload bytes.
    pub fn encode_into<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        write_skippable_frame_to(self.magic_variant, &self.payload, writer).map(|_| ())
    }

    /// Read one skippable frame from `reader`. Consumes
    /// 4-byte magic + 4-byte length + `length` payload bytes. The
    /// caller is responsible for positioning the reader at a frame
    /// boundary; this method does not scan past unknown content.
    ///
    /// Three layers of protection against crafted-`length` DoS:
    ///
    /// 1. Validates that `length` is representable on the target
    ///    pointer width (`length + SKIPPABLE_HEADER_SIZE` must not
    ///    overflow `usize`). On 32-bit targets a wire `length` near
    ///    `u32::MAX` would otherwise overflow `serialized_size()` and
    ///    `write_skippable_frame_to`. Returns
    ///    [`DecodeSkippableFrameError::PayloadTooLarge`] up front.
    ///
    /// 2. Reserves the address space via [`Vec::try_reserve_exact`],
    ///    converting alloc-failure into typed
    ///    [`DecodeSkippableFrameError::AllocationFailed`] instead of
    ///    process abort.
    ///
    /// 3. Reads the payload in fixed-size chunks via a stack scratch
    ///    buffer, so the OS only commits pages for bytes the reader
    ///    actually delivers. A crafted `length` near `u32::MAX` on a
    ///    reader that terminates early surfaces as
    ///    `DecodeSkippableFrameError::Payload` without ever
    ///    committing the full allocation — on OSes with memory
    ///    overcommit (Linux default) where step 2 would otherwise
    ///    succeed for any nominal size, this is what makes the
    ///    "no abort on huge length" guarantee actually reliable.
    ///
    /// Callers handling untrusted streams should additionally cap
    /// the acceptable payload size at the application layer; this
    /// method itself imposes no upper bound beyond the wire-format
    /// `u32::MAX` plus target-representability.
    pub fn decode_from<R: Read>(reader: &mut R) -> Result<Self, DecodeSkippableFrameError> {
        let mut magic_buf = [0u8; 4];
        reader
            .read_exact(&mut magic_buf)
            .map_err(DecodeSkippableFrameError::Magic)?;
        let magic_number = u32::from_le_bytes(magic_buf);

        let variant = magic_number.wrapping_sub(SKIPPABLE_MAGIC_START);
        if !(0..=u32::from(SKIPPABLE_MAGIC_MAX_VARIANT)).contains(&variant) {
            return Err(DecodeSkippableFrameError::BadMagicNumber(magic_number));
        }

        let mut len_buf = [0u8; 4];
        reader
            .read_exact(&mut len_buf)
            .map_err(DecodeSkippableFrameError::Length)?;
        let length_u32 = u32::from_le_bytes(len_buf);

        // Convert the wire-format u32 length to `usize` via
        // `TryFrom` (NOT `as usize`). On 16-bit pointer-width
        // targets (e.g. MSP430) the bare `as usize` would silently
        // truncate any value above `u16::MAX`, leaving the
        // subsequent allocation + `read_exact` to consume far fewer
        // bytes than the wire declared and leaving the reader
        // mis-aligned at a junk position in the stream. Surface
        // unrepresentable lengths as `PayloadTooLarge` BEFORE any
        // allocation; the `requested` payload is reported in u32
        // wire-format units so callers can still see the declared
        // value even when it does not fit in this target's
        // `usize`.
        let length = usize::try_from(length_u32).map_err(|_| {
            DecodeSkippableFrameError::PayloadTooLarge {
                length: length_u32 as usize, // saturates to usize::MAX on truncation, for diagnostic only
            }
        })?;

        // Reject lengths that the `new()` / `write_skippable_frame()`
        // path would also reject up front. On 32-bit targets this
        // catches `length + SKIPPABLE_HEADER_SIZE` overflowing
        // `usize` when the declared length sits near `u32::MAX`.
        // On 64-bit the check is a no-op (every u32 length is
        // representable). On 16-bit the upstream `try_from` already
        // rejected everything above `u16::MAX`, so this is also
        // a no-op there.
        if length.checked_add(SKIPPABLE_HEADER_SIZE).is_none() {
            return Err(DecodeSkippableFrameError::PayloadTooLarge { length });
        }

        let mut payload: Vec<u8> = Vec::new();
        payload
            .try_reserve_exact(length)
            .map_err(|_| DecodeSkippableFrameError::AllocationFailed { requested: length })?;

        // Read in chunks via a stack scratch buffer instead of
        // `resize(length, 0) + read_exact(&mut payload)`. The
        // resize-then-read path eagerly zero-fills the entire
        // address range up front, which on overcommit OSes
        // (Linux default) triggers the OOM killer the moment the
        // crafted-`length` worth of pages get committed — even
        // though `try_reserve_exact` succeeded earlier. Chunked
        // reads commit pages only as the reader delivers bytes,
        // so a 4 GiB-declared payload on a 12-byte stream commits
        // ~one page, surfaces `Payload`, and exits.
        const CHUNK: usize = 16 * 1024;
        let mut scratch = [0u8; CHUNK];
        let mut remaining = length;
        while remaining > 0 {
            let take = remaining.min(CHUNK);
            reader
                .read_exact(&mut scratch[..take])
                .map_err(DecodeSkippableFrameError::Payload)?;
            payload.extend_from_slice(&scratch[..take]);
            remaining -= take;
        }

        Ok(Self {
            magic_variant: variant as u8,
            payload,
        })
    }
}

/// Free function for callers that want to write a skippable frame
/// directly into a sink without constructing a temporary
/// [`SkippableFrame`]. Shape mirrors donor
/// `ZSTD_writeSkippableFrame(dst, dstCapacity, src, srcSize,
/// magicVariant)` — same validation, same byte-level output.
///
/// On success returns the number of bytes written
/// (`payload.len() + 8`).
pub fn write_skippable_frame<W: Write>(
    magic_variant: u8,
    payload: &[u8],
    writer: &mut W,
) -> Result<usize, SkippableFrameError> {
    validate_magic_variant(magic_variant)?;
    validate_payload_size(payload.len())?;
    write_skippable_frame_to(magic_variant, payload, writer).map_err(SkippableFrameError::Io)
}

/// Internal raw writer. Skips validation (caller must have validated
/// `magic_variant` and `payload.len()` first) and propagates raw I/O
/// errors. Used by both the typed [`SkippableFrame::encode_into`] and
/// the free [`write_skippable_frame`].
fn write_skippable_frame_to<W: Write>(
    magic_variant: u8,
    payload: &[u8],
    writer: &mut W,
) -> Result<usize, Error> {
    let magic = SKIPPABLE_MAGIC_START + u32::from(magic_variant);
    let length = payload.len() as u32;

    writer.write_all(&magic.to_le_bytes())?;
    writer.write_all(&length.to_le_bytes())?;
    writer.write_all(payload)?;
    Ok(payload.len() + SKIPPABLE_HEADER_SIZE)
}

#[inline]
fn validate_magic_variant(magic_variant: u8) -> Result<(), SkippableFrameError> {
    if magic_variant > SKIPPABLE_MAGIC_MAX_VARIANT {
        Err(SkippableFrameError::InvalidMagicVariant(magic_variant))
    } else {
        Ok(())
    }
}

#[inline]
fn validate_payload_size(len: usize) -> Result<(), SkippableFrameError> {
    // The on-wire length field is u32; payloads beyond u32::MAX are
    // not representable. The `as u64` cast is needed to compare on
    // 32-bit targets where `u32::MAX as usize == usize::MAX` and the
    // condition trivially folds away (correct: no payload on 32-bit
    // can exceed the limit).
    if (len as u64) > u64::from(u32::MAX) {
        return Err(SkippableFrameError::PayloadTooLarge(len));
    }
    // On 32-bit targets `usize` IS `u32` so the wire-format limit
    // (`u32::MAX`) is identical to `usize::MAX`. Computing the total
    // serialised size as `len + SKIPPABLE_HEADER_SIZE` would then
    // overflow `usize` when `len` sits at the wire-format ceiling.
    // Reject those borderline-sized payloads up front so
    // `serialized_size()` and `write_skippable_frame_to` stay
    // unconditionally panic-free across target widths.
    if len.checked_add(SKIPPABLE_HEADER_SIZE).is_none() {
        return Err(SkippableFrameError::PayloadTooLarge(len));
    }
    Ok(())
}

/// Errors surfaced when constructing or writing a [`SkippableFrame`].
#[derive(Debug)]
#[non_exhaustive]
pub enum SkippableFrameError {
    /// `magic_variant` outside the spec's `0..=15` range.
    InvalidMagicVariant(u8),
    /// `payload.len()` exceeds `u32::MAX`, the on-wire length field
    /// width, OR would overflow `usize` when combined with the
    /// 8-byte skippable-frame header (32-bit targets).
    PayloadTooLarge(usize),
    /// Underlying I/O error from the writer.
    Io(Error),
}

/// Errors surfaced when reading a [`SkippableFrame`] from a stream.
#[derive(Debug)]
#[non_exhaustive]
pub enum DecodeSkippableFrameError {
    /// I/O error while reading the 4-byte magic prefix.
    Magic(Error),
    /// First 4 bytes are not a skippable-frame magic in the
    /// `0x184D2A50..=0x184D2A5F` range.
    BadMagicNumber(u32),
    /// I/O error while reading the 4-byte length field.
    Length(Error),
    /// I/O error while reading the payload bytes.
    Payload(Error),
    /// Allocation of the payload buffer failed (e.g. a crafted
    /// length field requested more memory than is available).
    /// `requested` is the byte count the on-wire length field
    /// asked for.
    AllocationFailed { requested: usize },
    /// Wire-format `length` field is not representable on this
    /// target's `usize` width: `length + SKIPPABLE_HEADER_SIZE`
    /// would overflow. Hits only 32-bit targets where the u32
    /// wire-format ceiling coincides with `usize::MAX`. On 64-bit
    /// every u32 length is representable and this variant is
    /// unreachable.
    PayloadTooLarge { length: usize },
}

impl core::fmt::Display for SkippableFrameError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidMagicVariant(v) => {
                write!(f, "skippable frame magic_variant {v} out of range 0..=15")
            }
            Self::PayloadTooLarge(n) => write!(
                f,
                "skippable frame payload size {n} not representable: either exceeds u32::MAX (wire-format length-field ceiling) or overflows usize when combined with the 8-byte header (32-bit targets)"
            ),
            Self::Io(e) => write!(f, "skippable frame I/O error: {e}"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for SkippableFrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::InvalidMagicVariant(_) | Self::PayloadTooLarge(_) => None,
        }
    }
}

impl From<Error> for SkippableFrameError {
    fn from(value: Error) -> Self {
        Self::Io(value)
    }
}

impl core::fmt::Display for DecodeSkippableFrameError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Magic(e) => write!(f, "skippable frame: error reading magic number: {e}"),
            Self::BadMagicNumber(m) => write!(
                f,
                "skippable frame: magic 0x{m:08X} is not in the skippable range 0x184D2A50..=0x184D2A5F"
            ),
            Self::Length(e) => write!(f, "skippable frame: error reading length field: {e}"),
            Self::Payload(e) => write!(f, "skippable frame: error reading payload bytes: {e}"),
            Self::AllocationFailed { requested } => write!(
                f,
                "skippable frame: failed to allocate {requested} bytes for payload"
            ),
            Self::PayloadTooLarge { length } => write!(
                f,
                "skippable frame: declared length {length} not representable on this target (length + 8 byte header overflows usize)"
            ),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for DecodeSkippableFrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Magic(e) | Self::Length(e) | Self::Payload(e) => Some(e),
            Self::BadMagicNumber(_)
            | Self::AllocationFailed { .. }
            | Self::PayloadTooLarge { .. } => None,
        }
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    fn build_donor_skippable(magic_variant: u8, payload: &[u8]) -> Vec<u8> {
        // Donor `ZSTD_writeSkippableFrame` (zstd v1.5.7
        // `lib/compress/zstd_compress.c:4751-4763`) emits exactly
        // `4-byte LE magic || 4-byte LE size || payload`. Mirror that
        // here as the byte-parity oracle. Re-implementing the donor
        // layout in the test (rather than calling out to zstd-sys)
        // keeps this test independent of the dev-dep wiring and
        // makes the parity expectation visible inline.
        let magic = (SKIPPABLE_MAGIC_START + u32::from(magic_variant)).to_le_bytes();
        let size = (payload.len() as u32).to_le_bytes();
        let mut out = Vec::with_capacity(payload.len() + SKIPPABLE_HEADER_SIZE);
        out.extend_from_slice(&magic);
        out.extend_from_slice(&size);
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn round_trip_all_sixteen_variants() {
        for variant in 0u8..=15 {
            let payload = alloc::vec![variant; 32 + variant as usize];
            let frame = SkippableFrame::new(variant, payload.clone()).expect("variant in range");
            let mut wire = Vec::new();
            frame
                .encode_into(&mut wire)
                .expect("encode into Vec succeeds");

            let mut cursor: &[u8] = wire.as_slice();
            let decoded = SkippableFrame::decode_from(&mut cursor).expect("round-trip decode");
            assert_eq!(decoded.magic_variant(), variant);
            assert_eq!(
                decoded.magic_number(),
                SKIPPABLE_MAGIC_START + u32::from(variant)
            );
            assert_eq!(decoded.payload(), payload.as_slice());
            assert!(
                cursor.is_empty(),
                "decode_from must consume exactly the frame bytes, no overshoot or undershoot"
            );
        }
    }

    #[test]
    fn empty_payload_round_trips() {
        let frame = SkippableFrame::new(7, Vec::new()).expect("empty payload OK");
        assert_eq!(frame.serialized_size(), SKIPPABLE_HEADER_SIZE);

        let mut wire = Vec::new();
        frame.encode_into(&mut wire).unwrap();
        assert_eq!(wire.len(), SKIPPABLE_HEADER_SIZE);

        let mut cursor: &[u8] = wire.as_slice();
        let decoded = SkippableFrame::decode_from(&mut cursor).unwrap();
        assert!(decoded.payload().is_empty());
        assert_eq!(decoded.magic_variant(), 7);
    }

    #[test]
    fn large_payload_round_trips() {
        // 1 MiB so the 4-byte length field carries a non-trivial
        // value (0x100000) — the byte-parity test below verifies the
        // LE serialisation explicitly.
        let payload = alloc::vec![0xABu8; 1024 * 1024];
        let frame = SkippableFrame::new(0, payload.clone()).unwrap();
        let mut wire = Vec::new();
        frame.encode_into(&mut wire).unwrap();
        assert_eq!(wire.len(), payload.len() + SKIPPABLE_HEADER_SIZE);

        let mut cursor: &[u8] = wire.as_slice();
        let decoded = SkippableFrame::decode_from(&mut cursor).unwrap();
        assert_eq!(decoded.payload().len(), payload.len());
        assert!(decoded.payload() == payload.as_slice());
    }

    #[test]
    fn new_rejects_variant_sixteen() {
        let err = SkippableFrame::new(16, Vec::new()).expect_err("variant 16 out of range");
        match err {
            SkippableFrameError::InvalidMagicVariant(v) => assert_eq!(v, 16),
            other => panic!("expected InvalidMagicVariant(16), got {other:?}"),
        }
    }

    #[test]
    fn new_rejects_variant_max() {
        // u8::MAX = 255 — clearly outside the spec's 0..=15 range.
        let err = SkippableFrame::new(255, Vec::new()).unwrap_err();
        match err {
            SkippableFrameError::InvalidMagicVariant(v) => assert_eq!(v, 255),
            other => panic!("expected InvalidMagicVariant(255), got {other:?}"),
        }
    }

    #[test]
    fn write_function_rejects_invalid_variant() {
        let mut sink: Vec<u8> = Vec::new();
        let err = write_skippable_frame(16, b"x", &mut sink).unwrap_err();
        assert!(matches!(err, SkippableFrameError::InvalidMagicVariant(16)));
        assert!(
            sink.is_empty(),
            "no bytes must be written on rejected input"
        );
    }

    #[test]
    fn byte_parity_with_donor_layout() {
        // For every variant + a handful of payload sizes, our output
        // bytes must equal the donor's `ZSTD_writeSkippableFrame`
        // layout byte-for-byte. This locks the wire-format contract
        // against future drift.
        for &payload_len in &[0usize, 1, 8, 256, 4096] {
            let payload: Vec<u8> = (0..payload_len).map(|i| (i % 251) as u8).collect();
            for variant in 0u8..=15 {
                let expected = build_donor_skippable(variant, &payload);

                let mut via_struct = Vec::new();
                SkippableFrame::new(variant, payload.clone())
                    .unwrap()
                    .encode_into(&mut via_struct)
                    .unwrap();
                assert_eq!(
                    via_struct, expected,
                    "struct encode mismatch: variant={variant} len={payload_len}"
                );

                let mut via_free = Vec::new();
                let written = write_skippable_frame(variant, &payload, &mut via_free).unwrap();
                assert_eq!(written, expected.len());
                assert_eq!(
                    via_free, expected,
                    "free-fn encode mismatch: variant={variant} len={payload_len}"
                );
            }
        }
    }

    #[test]
    fn decode_rejects_non_skippable_magic() {
        // Zstd-1 magic 0xFD2FB528 is NOT in the skippable range.
        let mut wire = Vec::new();
        wire.extend_from_slice(&0xFD2F_B528u32.to_le_bytes());
        wire.extend_from_slice(&0u32.to_le_bytes());
        let mut cursor: &[u8] = wire.as_slice();
        let err = SkippableFrame::decode_from(&mut cursor).unwrap_err();
        match err {
            DecodeSkippableFrameError::BadMagicNumber(m) => assert_eq!(m, 0xFD2F_B528),
            other => panic!("expected BadMagicNumber, got {other:?}"),
        }
    }

    #[test]
    fn decode_rejects_magic_above_band() {
        // 0x184D2A60 is one past the skippable band — must be
        // rejected via BadMagicNumber, not silently accepted as
        // variant 16.
        let mut wire = Vec::new();
        wire.extend_from_slice(&0x184D_2A60u32.to_le_bytes());
        wire.extend_from_slice(&0u32.to_le_bytes());
        let mut cursor: &[u8] = wire.as_slice();
        let err = SkippableFrame::decode_from(&mut cursor).unwrap_err();
        assert!(matches!(
            err,
            DecodeSkippableFrameError::BadMagicNumber(0x184D_2A60)
        ));
    }

    #[test]
    fn decode_truncated_magic_surfaces_typed_error() {
        // Three bytes (one less than a magic) — must fail on the
        // magic read step, not panic.
        let wire = [0x50u8, 0x2A, 0x4D];
        let mut cursor: &[u8] = &wire;
        let err = SkippableFrame::decode_from(&mut cursor).unwrap_err();
        assert!(
            matches!(err, DecodeSkippableFrameError::Magic(_)),
            "expected Magic, got {err:?}"
        );
    }

    #[test]
    fn decode_truncated_length_surfaces_typed_error() {
        // Magic OK, but length field is short (3 bytes instead of 4).
        let mut wire = Vec::new();
        wire.extend_from_slice(&SKIPPABLE_MAGIC_START.to_le_bytes());
        wire.extend_from_slice(&[0u8, 0, 0]);
        let mut cursor: &[u8] = wire.as_slice();
        let err = SkippableFrame::decode_from(&mut cursor).unwrap_err();
        assert!(
            matches!(err, DecodeSkippableFrameError::Length(_)),
            "expected Length, got {err:?}"
        );
    }

    #[test]
    fn decode_truncated_payload_surfaces_typed_error() {
        // Header claims 16-byte payload but only 4 bytes follow.
        // The error must point at the PAYLOAD read step, not get
        // misreported as a header / descriptor read.
        let mut wire = Vec::new();
        wire.extend_from_slice(&SKIPPABLE_MAGIC_START.to_le_bytes());
        wire.extend_from_slice(&16u32.to_le_bytes());
        wire.extend_from_slice(&[0u8; 4]);
        let mut cursor: &[u8] = wire.as_slice();
        let err = SkippableFrame::decode_from(&mut cursor).unwrap_err();
        assert!(
            matches!(err, DecodeSkippableFrameError::Payload(_)),
            "expected Payload, got {err:?}"
        );
    }

    #[test]
    fn serialized_size_matches_encoded_length() {
        for payload_len in [0usize, 1, 7, 8, 9, 255, 256, 1023, 1024] {
            let payload = alloc::vec![0u8; payload_len];
            let frame = SkippableFrame::new(3, payload).unwrap();
            let mut wire = Vec::new();
            frame.encode_into(&mut wire).unwrap();
            assert_eq!(
                wire.len(),
                frame.serialized_size(),
                "serialized_size() must match actual encode length for payload_len={payload_len}"
            );
        }
    }

    #[test]
    fn decode_huge_length_returns_typed_error_not_oom_abort() {
        // Crafted wire declares a u32::MAX payload but provides
        // zero payload bytes. The decoder must surface a typed
        // error rather than aborting the process or panicking.
        // Three paths are acceptable, each gated by the host's
        // ABI / allocator behaviour:
        //
        // - `PayloadTooLarge { length }` — 32-bit host, where
        //   `length + 8` overflows `usize`. The decoder rejects
        //   the length before allocating.
        // - `AllocationFailed { requested }` — 64-bit host, no
        //   memory overcommit (Windows / configured Linux):
        //   `try_reserve_exact` reports failure.
        // - `Payload(io_err)` — 64-bit host, memory overcommit
        //   (Linux default / macOS): allocation succeeds for the
        //   address range, chunked read on truncated stream
        //   surfaces the I/O error after committing one page
        //   for the scratch buffer.
        //
        // What it must NOT do: abort the process on OOM or panic
        // via Vec::with_capacity / Vec::resize.
        let huge: u32 = u32::MAX;
        let mut wire = Vec::new();
        wire.extend_from_slice(&SKIPPABLE_MAGIC_START.to_le_bytes());
        wire.extend_from_slice(&huge.to_le_bytes());
        let mut cursor: &[u8] = wire.as_slice();
        let err = SkippableFrame::decode_from(&mut cursor).unwrap_err();
        match err {
            DecodeSkippableFrameError::PayloadTooLarge { length } => {
                assert_eq!(length, huge as usize);
            }
            DecodeSkippableFrameError::AllocationFailed { requested } => {
                assert_eq!(requested, huge as usize);
            }
            DecodeSkippableFrameError::Payload(_) => {
                // Chunked read on the truncated payload surfaced
                // the I/O error after the OS overcommitted the
                // address range. Also acceptable.
            }
            other => panic!("expected PayloadTooLarge / AllocationFailed / Payload, got {other:?}"),
        }
    }

    #[test]
    fn payload_too_large_check_branches_on_pointer_width() {
        // The `validate_payload_size` invariant is twofold:
        //
        // 1. `len > u32::MAX` is rejected on every target (the
        //    on-wire length field is u32).
        // 2. `len + SKIPPABLE_HEADER_SIZE` overflowing `usize` is
        //    rejected on every target. On 64-bit this is
        //    unreachable because `u32::MAX + 8 < usize::MAX`. On
        //    32-bit `len == u32::MAX` itself trips condition 2:
        //    `u32::MAX + 8` wraps `usize`.
        //
        // Branch the boundary expectation on pointer width so the
        // test passes on both i686 (CI cross-i686 shard) and
        // x86_64 hosts.
        #[cfg(target_pointer_width = "64")]
        {
            let result = validate_payload_size(u32::MAX as usize + 1);
            assert!(matches!(
                result,
                Err(SkippableFrameError::PayloadTooLarge(_))
            ));
            let ok = validate_payload_size(u32::MAX as usize);
            assert!(ok.is_ok(), "u32::MAX representable on 64-bit");
        }

        #[cfg(target_pointer_width = "32")]
        {
            // `u32::MAX + 1` literally cannot be expressed as
            // `usize` on 32-bit — `u32::MAX as usize + 1` wraps
            // to 0. So construct the test only through values
            // that are validly representable.
            let result = validate_payload_size(u32::MAX as usize);
            assert!(
                matches!(result, Err(SkippableFrameError::PayloadTooLarge(_))),
                "u32::MAX overflows when combined with the 8-byte header on 32-bit"
            );
            let ok = validate_payload_size((u32::MAX as usize) - SKIPPABLE_HEADER_SIZE);
            assert!(ok.is_ok(), "below the header-overflow boundary on 32-bit");
        }
    }
}
